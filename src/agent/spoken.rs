//! Turn an agent's final message into the short text whisrs speaks.
//!
//! The voice instruction asks the model to end with a `<spoken>` reply, but
//! users run different models, some small, so nothing here trusts it:
//!
//! 1. Take the last complete `<spoken>...</spoken>` pair (a tag mentioned
//!    in the text after it does not count). Without one, take the whole
//!    reply and keep its first two sentences.
//! 2. Sanitize: fenced code dropped; inline code of more than three words
//!    dropped, shorter kept as words; `[text](url)` becomes the text; bare
//!    URLs and file paths dropped; markdown markers, list bullets and table
//!    rows stripped; whitespace collapsed.
//! 3. Cap at [`MAX_SPOKEN_CHARS`] on a sentence boundary, so every TTS
//!    backend takes it in one request.
//! 4. Nothing speakable after all that (no letter or digit left):
//!    [`EMPTY_REPLY`].
//!
//! [`spoken_error`] does the same for a failed turn's error message, and
//! [`interim_line`] for an acknowledgement or progress update the agent
//! wrote before a tool call.
//!
//! Pure functions, no I/O.

use std::sync::LazyLock;

use regex::Regex;

/// The longest reply spoken, in characters.
pub const MAX_SPOKEN_CHARS: usize = 400;

/// What is spoken when nothing speakable is left.
pub const EMPTY_REPLY: &str = "Done.";

/// What is spoken for an error with nothing speakable in it.
pub const ERROR_FALLBACK: &str = "The agent turn failed.";

/// The longest interim line (an acknowledgement or a progress update)
/// spoken, in characters.
pub const MAX_INTERIM_CHARS: usize = 120;

/// Sentences kept from a reply without a `<spoken>` block.
const FALLBACK_SENTENCES: usize = 2;

/// Inline code longer than this many words is dropped instead of read.
const INLINE_CODE_MAX_WORDS: usize = 3;

/// The spoken form of an agent's final message.
pub fn spoken_reply(reply: &str) -> String {
    let text = match last_spoken_block(reply) {
        Some(block) => sanitize(block),
        None => first_sentences(&sanitize(&strip_spoken_tags(reply)), FALLBACK_SENTENCES),
    };
    let capped = cap_on_sentence(&text, MAX_SPOKEN_CHARS);
    if speakable(&capped) {
        capped
    } else {
        EMPTY_REPLY.to_string()
    }
}

/// Whether `text` has anything to say: a letter or a digit, not only the
/// punctuation a dropped URL or path left behind.
fn speakable(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

/// The spoken form of an interim message: the text of an assistant message
/// a tool call followed, so known not to be the final reply. Only a complete
/// `<spoken>...</spoken>` pair counts (an interim has no fallback: text
/// without one is the agent's own notes), sanitized like a reply and capped
/// at [`MAX_INTERIM_CHARS`] on a sentence or word boundary. `None` when
/// nothing speakable is left.
pub fn interim_line(raw: &str) -> Option<String> {
    let block = last_complete_spoken_block(raw)?;
    let line = cap_on_sentence(&sanitize(block), MAX_INTERIM_CHARS);
    speakable(&line).then_some(line)
}

/// Two raw messages that are the same: equal up to whitespace (the
/// transcript's text of a turn that ended on a poll can differ from the
/// Stop hook's in whitespace). Also how interim lines are deduplicated.
pub fn same_reply(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// The short line spoken for a failed turn: the first sentence of the error
/// message, without its `Agent '<name>': ` prefix, sanitized like a reply
/// (paths, URLs and markup dropped) and capitalized.
pub fn spoken_error(message: &str) -> String {
    let body = message
        .strip_prefix("Agent '")
        .and_then(|rest| rest.split_once("': "))
        .map_or(message, |(_, rest)| rest);
    let line = first_sentences(&sanitize(body), 1);
    let line = cap_on_sentence(&line, MAX_SPOKEN_CHARS);
    if !speakable(&line) {
        return ERROR_FALLBACK.to_string();
    }
    let mut chars = line.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => ERROR_FALLBACK.to_string(),
    }
}

const OPEN_TAG: &str = "<spoken>";
const CLOSE_TAG: &str = "</spoken>";

/// The content of the last complete `<spoken>...</spoken>` pair (tags
/// matched ASCII-case-insensitively): each closing tag pairs with the last
/// opening tag before it, so a tag the reply mentions after its block
/// (`I used the <spoken> tag`) is not taken for one. Without a complete
/// pair, a block opened after the last closing tag and never closed (a
/// truncated reply) yields everything after its first opening tag. `None`
/// without either.
pub fn last_spoken_block(reply: &str) -> Option<&str> {
    let (last_pair, after_last_close) = scan_spoken_pairs(reply);
    if last_pair.is_some() {
        return last_pair;
    }
    let lower = reply[after_last_close..].to_ascii_lowercase();
    let open = lower.find(OPEN_TAG)?;
    Some(&reply[after_last_close + open + OPEN_TAG.len()..])
}

/// The content of the last complete `<spoken>...</spoken>` pair, paired as
/// in [`last_spoken_block`]. `None` without one: a truncated block does not
/// count.
pub fn last_complete_spoken_block(text: &str) -> Option<&str> {
    scan_spoken_pairs(text).0
}

/// The last complete pair's content, and where the text after the last
/// closing tag starts (0 without one).
fn scan_spoken_pairs(reply: &str) -> (Option<&str>, usize) {
    // ASCII lowercasing keeps every byte offset valid in `reply`.
    let lower = reply.to_ascii_lowercase();
    let mut last_pair = None;
    // Where the text after the previous closing tag starts.
    let mut from = 0;
    while let Some(rel) = lower[from..].find(CLOSE_TAG) {
        let close = from + rel;
        if let Some(open) = lower[from..close].rfind(OPEN_TAG) {
            let start = from + open + OPEN_TAG.len();
            last_pair = Some(&reply[start..close]);
        }
        from = close + CLOSE_TAG.len();
    }
    (last_pair, from)
}

/// `reply` without any `<spoken>` or `</spoken>` tag.
fn strip_spoken_tags(reply: &str) -> String {
    static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</?spoken>").unwrap());
    TAGS.replace_all(reply, " ").into_owned()
}

/// Strip what a listener cannot use from markdown-ish text. See the module
/// docs for the rules.
pub fn sanitize(text: &str) -> String {
    let mut out = String::new();
    // The paragraph being joined from plain lines.
    let mut paragraph = String::new();
    let mut in_fence = false;

    let flush = |paragraph: &mut String, out: &mut String| {
        let p = finish_clause(paragraph);
        if !p.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&p);
        }
        paragraph.clear();
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            flush(&mut paragraph, &mut out);
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            flush(&mut paragraph, &mut out);
            continue;
        }
        // Table rows and their separators, horizontal rules, setext
        // underlines: nothing to say.
        if trimmed.starts_with('|') || is_rule(trimmed) {
            flush(&mut paragraph, &mut out);
            continue;
        }
        let (body, is_block) = strip_block_marker(trimmed);
        let words = inline(body);
        if words.is_empty() {
            continue;
        }
        if is_block {
            // A heading or a list item is a clause of its own.
            flush(&mut paragraph, &mut out);
            paragraph.push_str(&words);
            flush(&mut paragraph, &mut out);
        } else {
            if !paragraph.is_empty() {
                paragraph.push(' ');
            }
            paragraph.push_str(&words);
        }
    }
    flush(&mut paragraph, &mut out);
    tidy(&out)
}

/// A line of only `-`, `*`, `_` or `=` (three or more): a rule or a setext
/// heading underline.
fn is_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && (compact.chars().all(|c| c == '-')
            || compact.chars().all(|c| c == '*')
            || compact.chars().all(|c| c == '_')
            || compact.chars().all(|c| c == '='))
}

/// Strip a heading, blockquote or list marker. `true` when the line is a
/// heading or a list item.
fn strip_block_marker(line: &str) -> (&str, bool) {
    let mut line = line;
    // Blockquotes, possibly nested.
    while let Some(rest) = line.strip_prefix('>') {
        line = rest.trim_start();
    }
    if line.starts_with('#') {
        let rest = line.trim_start_matches('#');
        if rest.is_empty() || rest.starts_with(' ') {
            return (rest.trim(), true);
        }
    }
    for bullet in ["- ", "* ", "+ ", "\u{2022} "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            // A task list box.
            let rest = rest
                .strip_prefix("[ ] ")
                .or_else(|| rest.strip_prefix("[x] "))
                .or_else(|| rest.strip_prefix("[X] "))
                .unwrap_or(rest);
            return (rest.trim(), true);
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && digits <= 3 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return (rest.trim(), true);
        }
    }
    (line, false)
}

/// Inline markdown of one line, reduced to words.
fn inline(line: &str) -> String {
    static LINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"!?\[([^\]]*)\]\([^)]*\)").unwrap());
    static AUTOLINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<[a-zA-Z][a-zA-Z0-9+.-]*://[^>\s]*>").unwrap());
    static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`+([^`]*)`+").unwrap());
    static HTML_TAG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"</?[a-zA-Z][a-zA-Z0-9-]*[^>]*>").unwrap());

    let s = AUTOLINK.replace_all(line, " ");
    let s = LINK.replace_all(&s, "$1");
    let s = CODE.replace_all(&s, |caps: &regex::Captures| {
        let inner = caps[1].trim();
        if inner.split_whitespace().count() > INLINE_CODE_MAX_WORDS {
            " ".to_string()
        } else {
            inner.to_string()
        }
    });
    let s = HTML_TAG.replace_all(&s, " ");
    let s = s.replace("**", "").replace("~~", "").replace('*', "");

    let mut words: Vec<String> = Vec::new();
    for token in s.split_whitespace() {
        if let Some(kept) = speakable_token(token) {
            words.push(kept);
        }
    }
    words.join(" ")
}

/// A whitespace-separated token as it should be read: `None` when nothing of
/// it is left. A URL or a path is dropped, but the sentence punctuation after
/// it is kept, so "I edited src/main.rs." still ends its sentence.
fn speakable_token(token: &str) -> Option<String> {
    const LEADING: &[char] = &['(', '[', '{', '"', '\'', '\u{201c}', '\u{2018}'];
    const TRAILING: &[char] = &[
        ')', ']', '}', '"', '\'', ',', '.', ';', ':', '!', '?', '\u{201d}', '\u{2019}',
    ];
    let core_start = token.len() - token.trim_start_matches(LEADING).len();
    let core = token[core_start..].trim_end_matches(TRAILING);
    let trailing = &token[core_start + core.len()..];
    if is_url(core) || is_path(core) {
        let punct: String = trailing
            .chars()
            .filter(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'))
            .collect();
        return (!punct.is_empty()).then_some(punct);
    }
    // `_emphasis_` and stray backticks or hashes left by broken markup.
    let cleaned = token.trim_matches(|c| c == '_' || c == '`');
    let cleaned = cleaned.replace('`', "");
    (!cleaned.is_empty()).then_some(cleaned)
}

/// A URL: a scheme (`https://`, `file://`, ...) or a `www.` host.
fn is_url(core: &str) -> bool {
    let lower = core.to_ascii_lowercase();
    lower.contains("://") || lower.starts_with("www.")
}

/// A file path: absolute or relative (`/x`, `~/x`, `./x`, `../x`), Windows
/// style, two or more slashes, or one slash before a file extension
/// (`src/main.rs`). "and/or", "1/2" and "TCP/IP" are words.
fn is_path(core: &str) -> bool {
    if core.is_empty() {
        return false;
    }
    if core.starts_with('/')
        || core.starts_with("~/")
        || core.starts_with("./")
        || core.starts_with("../")
        || core.contains('\\')
    {
        return true;
    }
    let slashes = core.matches('/').count();
    if slashes >= 2 {
        return true;
    }
    if slashes == 1 {
        let last = core.rsplit('/').next().unwrap_or("");
        if let Some((stem, ext)) = last.rsplit_once('.') {
            return !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 5
                && ext.chars().all(|c| c.is_ascii_alphanumeric());
        }
    }
    false
}

/// End a clause with sentence punctuation unless it has some.
fn finish_clause(clause: &str) -> String {
    let c = tidy(clause);
    if c.is_empty() {
        return c;
    }
    let last = c.chars().last().unwrap_or('.');
    if matches!(last, '.' | '!' | '?' | ':' | ';' | ',') {
        if matches!(last, ':' | ';' | ',') {
            // A clause cut at a colon ("Changes:") before a list reads as a
            // sentence end.
            let mut c = c;
            c.pop();
            c.push('.');
            return c;
        }
        c
    } else {
        format!("{c}.")
    }
}

/// Collapse whitespace, and the gaps and doubled marks that dropping words
/// leaves behind.
fn tidy(text: &str) -> String {
    static SPACE_BEFORE_PUNCT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s+([.,;:!?])").unwrap());
    static EMPTY_BRACKETS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\(\s*\)|\[\s*\]").unwrap());
    static DOUBLED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([.,;:!?])[.,;:]+").unwrap());
    static LEADING_PUNCT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\s.,;:!?]+").unwrap());

    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = EMPTY_BRACKETS.replace_all(&collapsed, "");
    let s = SPACE_BEFORE_PUNCT.replace_all(&s, "$1");
    let s = DOUBLED.replace_all(&s, "$1");
    let s = LEADING_PUNCT.replace(&s, "");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split into sentences: each ends at `.`, `!` or `?` (and any closing quote
/// or bracket after it) followed by whitespace or the end. Text after the
/// last mark is a final sentence.
pub fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (_, c) = chars[i];
        if matches!(c, '.' | '!' | '?') {
            let mut j = i + 1;
            while j < chars.len() && matches!(chars[j].1, '.' | '!' | '?' | '"' | '\'' | ')') {
                j += 1;
            }
            let at_end = j >= chars.len();
            if at_end || chars[j].1.is_whitespace() {
                let end = if at_end { text.len() } else { chars[j].0 };
                let sentence = text[start..end].trim();
                if !sentence.is_empty() {
                    out.push(sentence);
                }
                start = end;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// The first `n` sentences of `text`.
pub fn first_sentences(text: &str, n: usize) -> String {
    sentences(text)
        .into_iter()
        .take(n)
        .collect::<Vec<_>>()
        .join(" ")
}

/// At most `max` characters of `text`, cut after the last sentence that
/// fits. A first sentence longer than `max` is cut at a word boundary and
/// ended with a period.
pub fn cap_on_sentence(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    for sentence in sentences(text) {
        let extra = if out.is_empty() { 0 } else { 1 };
        if out.chars().count() + extra + sentence.chars().count() > max {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(sentence);
    }
    if !out.is_empty() {
        return out;
    }
    // One sentence longer than the cap: cut it at a word boundary.
    let mut cut = String::new();
    for word in text.split_whitespace() {
        let extra = if cut.is_empty() { 0 } else { 1 };
        // Room for the closing period.
        if cut.chars().count() + extra + word.chars().count() + 1 > max {
            break;
        }
        if !cut.is_empty() {
            cut.push(' ');
        }
        cut.push_str(word);
    }
    if cut.is_empty() {
        // A single "word" longer than the cap.
        cut = text.chars().take(max.saturating_sub(1)).collect();
    }
    let cut = cut.trim_end_matches([',', ';', ':', '-']).to_string();
    format!("{cut}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_spoken_from_its_last_block_or_first_sentences() {
        for (name, reply, want) in [
            (
                "the last block wins",
                "Working.\n<spoken>First draft.</spoken>\nMore notes.\n\
                 <spoken>I fixed the failing test and pushed it.</spoken>\nDetails below.",
                "I fixed the failing test and pushed it.",
            ),
            (
                "tags in any case",
                "x <SPOKEN>All done.</Spoken> y",
                "All done.",
            ),
            // A clause without an end mark gets one, which TTS reads as a stop.
            (
                "an unclosed block",
                "notes <spoken>It builds now",
                "It builds now.",
            ),
            (
                "no block: the first two sentences",
                "I renamed the function. Tests pass. I also updated the docs. Anything else?",
                "I renamed the function. Tests pass.",
            ),
            (
                "markdown, no block",
                "## Summary\n\nFixed the **race** in `claim_session`:\n\n\
                 - moved the check\n- added a test\n\n```rust\nfn x() {}\n```\n",
                "Summary. Fixed the race in claim_session.",
            ),
            (
                "a block is sanitized",
                "<spoken>**Done.** I changed `main.rs` in src/main.rs.</spoken>",
                "Done. I changed main.rs in.",
            ),
            (
                "a stray closing tag",
                "All good.</spoken> Next.",
                "All good. Next.",
            ),
            // Nothing speakable says done.
            ("empty", "", EMPTY_REPLY),
            ("an empty block", "<spoken></spoken>", EMPTY_REPLY),
            ("code only", "```\ncode only\n```", EMPTY_REPLY),
            (
                "a URL and a path",
                "<spoken>https://x.io /a/b</spoken>",
                EMPTY_REPLY,
            ),
            // Punctuation a dropped URL leaves behind is nothing to say.
            (
                "a URL's punctuation",
                "<spoken>https://x.io!</spoken>",
                EMPTY_REPLY,
            ),
            (
                "a path's punctuation",
                "<spoken>/a/b?</spoken>",
                EMPTY_REPLY,
            ),
            (
                "a URL's punctuation, then a sentence",
                "<spoken>https://x.io! Then it built.</spoken>",
                "Then it built.",
            ),
        ] {
            assert_eq!(spoken_reply(reply), want, "{name}");
        }
    }

    #[test]
    fn sanitize_reads_markdown_as_words() {
        for (name, text, want) in [
            (
                "fenced code",
                "Before.\n```bash\nrm -rf target\necho hi\n```\nAfter.",
                "Before. After.",
            ),
            // An unterminated fence drops the rest.
            (
                "an unterminated fence",
                "Run this:\n~~~\ncargo build\n",
                "Run this.",
            ),
            (
                "short inline code",
                "Use `cargo test` here.",
                "Use cargo test here.",
            ),
            (
                "long inline code",
                "Run `cargo test --all-targets -- --nocapture` again.",
                "Run again.",
            ),
            (
                "a link and a URL",
                "See [the docs](https://example.com/docs) or https://x.io/a?b=c for more.",
                "See the docs or for more.",
            ),
            ("an autolink", "Visit <https://example.com>.", "Visit."),
            ("a www host", "Go to www.example.com now.", "Go to now."),
            (
                "paths",
                "I edited src/daemon/agent.rs and ~/notes.md.",
                "I edited and.",
            ),
            (
                "a path at the sentence end",
                "Changed src/main.rs.",
                "Changed.",
            ),
            (
                "an absolute path",
                "Wrote /tmp/out, then stopped.",
                "Wrote, then stopped.",
            ),
            // Words with one slash and no extension stay.
            (
                "one slash, no extension",
                "Use and/or TCP/IP, 1/2 of it.",
                "Use and/or TCP/IP, 1/2 of it.",
            ),
            (
                "markers, bullets and tables",
                "# Title\n> quoted *text*\n1. first\n2) second\n* third\n\
                 | a | b |\n|---|---|\n| 1 | 2 |\n---\n__done__",
                "Title. quoted text. first. second. third. done.",
            ),
            ("whitespace", "  a\t\tb  \n  c  ", "a b c."),
        ] {
            assert_eq!(sanitize(text), want, "{name}");
        }
    }

    #[test]
    fn the_cap_cuts_on_a_sentence_boundary() {
        let sentence = "This sentence is exactly fifty characters long ok.";
        assert_eq!(sentence.chars().count(), 50);
        let text = [sentence; 10].join(" ");
        let capped = cap_on_sentence(&text, 400);
        assert!(capped.chars().count() <= 400);
        assert!(capped.ends_with("long ok."));
        // Eight sentences and seven spaces: 407 > 400, so seven fit.
        assert_eq!(sentences(&capped).len(), 7);
        let reply = format!("<spoken>{text}</spoken>");
        assert_eq!(spoken_reply(&reply), capped);
    }

    #[test]
    fn one_long_sentence_is_cut_at_a_word() {
        let text = "word ".repeat(200);
        let capped = cap_on_sentence(&text, 400);
        assert!(capped.chars().count() <= 400, "{}", capped.len());
        assert!(capped.ends_with("word."));
        let unbroken = "x".repeat(500);
        assert_eq!(cap_on_sentence(&unbroken, 400).chars().count(), 400);
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        let text = "Ünïcödé wörds hëre. ".repeat(30);
        let capped = cap_on_sentence(&text, 400);
        assert!(capped.chars().count() <= 400);
        assert!(capped.ends_with('.'));
    }

    #[test]
    fn sentences_split_on_marks_followed_by_space() {
        assert_eq!(
            sentences("One. Two! Three? Version 1.5 works. Tail"),
            vec!["One.", "Two!", "Three?", "Version 1.5 works.", "Tail"]
        );
        assert_eq!(
            sentences("He said \"stop.\" Then left."),
            vec!["He said \"stop.\"", "Then left."]
        );
    }

    /// A tag the reply mentions after its block is not a block.
    #[test]
    fn the_last_complete_pair_wins_over_a_later_mention() {
        let reply = "<spoken>I fixed it.</spoken>\nI wrapped the reply in a <spoken> tag.";
        assert_eq!(last_spoken_block(reply), Some("I fixed it."));
        assert_eq!(spoken_reply(reply), "I fixed it.");
        let reply = "<spoken>Fixed.</spoken> Close it with </spoken> like this.";
        assert_eq!(spoken_reply(reply), "Fixed.");
        // Each close pairs with the nearest open before it.
        assert_eq!(
            last_spoken_block("<spoken>draft <spoken>final</spoken>"),
            Some("final")
        );
        // An unclosed block runs to the end.
        assert_eq!(
            last_spoken_block("notes <spoken>It builds now"),
            Some("It builds now")
        );
        // Only a truncated block: everything after its first tag, the
        // mentioned tag stripped.
        assert_eq!(
            spoken_reply("notes <spoken>It builds. I used the <spoken> tag"),
            "It builds. I used the tag."
        );
        // A truncated block after a complete one does not win.
        assert_eq!(
            spoken_reply("<spoken>Done with it.</spoken> more <spoken>cut off"),
            "Done with it."
        );
    }

    #[test]
    fn an_error_is_spoken_as_its_first_sentence() {
        assert_eq!(
            spoken_error("Agent 'jarvis': That turn was interrupted, say it again."),
            "That turn was interrupted, say it again."
        );
        assert_eq!(
            spoken_error(
                "Agent 'jarvis': `claude` was not found. Install Claude Code, or set command in \
                 [[agents]] to its full path."
            ),
            "Claude was not found."
        );
        assert_eq!(
            spoken_error("Agent 'jarvis': its dir /home/u/Projects/x does not exist."),
            "Its dir does not exist."
        );
        assert_eq!(
            spoken_error(
                "Agent 'jarvis': Claude Code does not trust this folder yet. Open /p in Claude \
                 Code once and accept the trust prompt."
            ),
            "Claude Code does not trust this folder yet."
        );
        assert_eq!(spoken_error("no prefix here. Second."), "No prefix here.");
        assert_eq!(spoken_error("Agent 'jarvis': /a/b/c"), ERROR_FALLBACK);
        assert_eq!(spoken_error(""), ERROR_FALLBACK);
    }

    #[test]
    fn an_interim_needs_a_complete_pair() {
        assert_eq!(
            interim_line("<spoken>On it.</spoken>\n\nLet me look at the config.").as_deref(),
            Some("On it.")
        );
        // The last complete pair, as for a reply.
        assert_eq!(
            interim_line("<spoken>On it.</spoken> then <SPOKEN>Found the cause.</Spoken>")
                .as_deref(),
            Some("Found the cause.")
        );
        // No pair, or only a truncated one: the agent's own notes.
        assert_eq!(interim_line("Let me check the logs."), None);
        assert_eq!(interim_line("notes <spoken>On it"), None);
        assert_eq!(last_complete_spoken_block("notes <spoken>On it"), None);
        assert_eq!(
            last_complete_spoken_block("<spoken>a</spoken> <spoken>cut"),
            Some("a")
        );
    }

    #[test]
    fn an_interim_is_sanitized_and_nothing_speakable_is_none() {
        assert_eq!(
            interim_line("<spoken>**Checking** `src/main.rs` now.</spoken>").as_deref(),
            Some("Checking now.")
        );
        assert_eq!(interim_line("<spoken></spoken>"), None);
        assert_eq!(interim_line("<spoken>https://x.io /a/b!</spoken>"), None);
        assert_eq!(interim_line("<spoken>```\ncode\n```</spoken>"), None);
    }

    #[test]
    fn an_interim_is_capped_on_a_sentence_or_a_word() {
        let first = "I found the cause in the cache layer.";
        let second = "The key ignored the locale, so every language shared one entry and \
                      the wrong strings came back.";
        let line = interim_line(&format!("<spoken>{first} {second}</spoken>")).unwrap();
        assert_eq!(line, first);
        let long = "word ".repeat(60);
        let line = interim_line(&format!("<spoken>{long}</spoken>")).unwrap();
        assert!(line.chars().count() <= MAX_INTERIM_CHARS, "{line}");
        assert!(line.ends_with("word."), "{line}");
    }

    #[test]
    fn replies_are_compared_up_to_whitespace() {
        assert!(same_reply(
            "Done.\n\n<spoken>All set.</spoken>",
            " Done. <spoken>All set.</spoken>\n"
        ));
        assert!(!same_reply("<spoken>Done.</spoken>", "Done."));
        assert!(!same_reply(
            "Fixed the test. <spoken>Done.</spoken>",
            "Ran it. <spoken>Done.</spoken>"
        ));
    }
}
