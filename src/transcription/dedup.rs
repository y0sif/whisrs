//! Timestamp-based deduplication for chunked streaming transcription.
//!
//! When sending audio in overlapping or consecutive chunks to a transcription
//! API, the same words may appear in multiple responses. This module tracks
//! the cumulative time offset and discards words that overlap with
//! already-transcribed ranges. Falls back to n-gram text matching when
//! timestamps are unavailable.

use tracing::debug;

use super::groq::GroqWord;

/// Tracks transcription progress across multiple chunks for deduplication.
pub struct DeduplicationTracker {
    /// The end time (in seconds) of the last word we accepted.
    transcribed_up_to: f64,
    /// Cumulative time offset added to each chunk's timestamps.
    cumulative_offset: f64,
    /// Recent text we've already output (for n-gram fallback).
    recent_text: String,
    /// Maximum number of characters to keep in `recent_text` for matching.
    max_recent_chars: usize,
}

impl DeduplicationTracker {
    /// Create a new tracker.
    pub fn new() -> Self {
        Self {
            transcribed_up_to: 0.0,
            cumulative_offset: 0.0,
            recent_text: String::new(),
            max_recent_chars: 500,
        }
    }

    /// Add a time offset for the next chunk (the duration of audio already sent).
    pub fn advance_offset(&mut self, chunk_duration_secs: f64) {
        self.cumulative_offset += chunk_duration_secs;
        debug!(
            "dedup: advanced offset by {:.2}s, cumulative = {:.2}s",
            chunk_duration_secs, self.cumulative_offset
        );
    }

    /// Filter words from a new chunk, returning only the non-duplicate ones.
    ///
    /// Each word's `start` and `end` are adjusted by the cumulative offset.
    /// Words whose adjusted `start` time falls within the already-transcribed
    /// range are discarded.
    pub fn filter_words(&mut self, words: &[GroqWord]) -> Vec<GroqWord> {
        let mut accepted = Vec::new();

        for word in words {
            let adjusted_start = word.start + self.cumulative_offset;
            let adjusted_end = word.end + self.cumulative_offset;

            if adjusted_start >= self.transcribed_up_to - 0.05 {
                // Accept this word.
                accepted.push(GroqWord {
                    word: word.word.clone(),
                    start: adjusted_start,
                    end: adjusted_end,
                });
                self.transcribed_up_to = adjusted_end;
            }
        }

        debug!(
            "dedup: accepted {}/{} words, transcribed_up_to = {:.2}s",
            accepted.len(),
            words.len(),
            self.transcribed_up_to
        );

        accepted
    }

    /// Filter text using n-gram overlap detection (fallback when timestamps
    /// are unavailable).
    ///
    /// Compares the beginning of `new_text` against the end of previously
    /// output text and removes the overlapping prefix.
    pub fn filter_text(&mut self, new_text: &str) -> String {
        let result = if self.recent_text.is_empty() {
            new_text.to_string()
        } else {
            remove_overlap(&self.recent_text, new_text)
        };

        // Update recent text buffer.
        self.recent_text.push_str(&result);
        if self.recent_text.len() > self.max_recent_chars {
            let trim_at = self.recent_text.len() - self.max_recent_chars;
            self.recent_text = self.recent_text[trim_at..].to_string();
        }

        result
    }
}

impl Default for DeduplicationTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Remove overlapping prefix between the end of `previous` and the start of `new`.
///
/// Tries increasingly longer suffixes of `previous` and checks if they match
/// the prefix of `new`. When a match is found (either exact or fuzzy),
/// returns `new` with the overlapping prefix removed.
fn remove_overlap(previous: &str, new: &str) -> String {
    let prev_words: Vec<&str> = previous.split_whitespace().collect();
    let new_words: Vec<&str> = new.split_whitespace().collect();

    if prev_words.is_empty() || new_words.is_empty() {
        return new.to_string();
    }

    // Try matching n-gram overlaps (from longest to shortest).
    // The sliding window approach can produce 15-20+ word overlaps,
    // so we use a generous limit here.
    let max_overlap = prev_words.len().min(new_words.len()).min(50);

    for overlap_len in (1..=max_overlap).rev() {
        let prev_suffix = &prev_words[prev_words.len() - overlap_len..];
        let new_prefix = &new_words[..overlap_len];

        if ngram_match(prev_suffix, new_prefix) {
            // Found overlap — return new text with the overlapping prefix removed.
            let remaining: Vec<&str> = new_words[overlap_len..].to_vec();
            if remaining.is_empty() {
                return String::new();
            }
            return remaining.join(" ");
        }
    }

    // No overlap found — return the full new text.
    new.to_string()
}

/// Check if two word slices match (allowing fuzzy matching per word).
fn ngram_match(a: &[&str], b: &[&str]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    a.iter().zip(b.iter()).all(|(wa, wb)| words_match(wa, wb))
}

/// Check if two words match, allowing for minor differences in punctuation
/// and small edit distances.
fn words_match(a: &str, b: &str) -> bool {
    // Normalize: lowercase and strip trailing punctuation.
    let na = normalize_word(a);
    let nb = normalize_word(b);

    if na == nb {
        return true;
    }

    // Use Jaro-Winkler similarity for fuzzy matching.
    let similarity = strsim::jaro_winkler(&na, &nb);
    similarity >= 0.85
}

/// Normalize a word for comparison: lowercase, strip trailing punctuation.
fn normalize_word(word: &str) -> String {
    word.to_lowercase()
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_dedup_no_overlap() {
        let mut tracker = DeduplicationTracker::new();

        let words = vec![
            GroqWord {
                word: "Hello".to_string(),
                start: 0.0,
                end: 0.5,
            },
            GroqWord {
                word: "world".to_string(),
                start: 0.6,
                end: 1.0,
            },
        ];

        let accepted = tracker.filter_words(&words);
        assert_eq!(accepted.len(), 2);
        assert_eq!(accepted[0].word, "Hello");
        assert_eq!(accepted[1].word, "world");
    }

    #[test]
    fn timestamp_dedup_skips_overlapping() {
        let mut tracker = DeduplicationTracker::new();

        // First chunk.
        let words1 = vec![
            GroqWord {
                word: "Hello".to_string(),
                start: 0.0,
                end: 0.5,
            },
            GroqWord {
                word: "world".to_string(),
                start: 0.6,
                end: 1.0,
            },
        ];
        let accepted1 = tracker.filter_words(&words1);
        assert_eq!(accepted1.len(), 2);

        // Second chunk with overlap — these words start before transcribed_up_to.
        let words2 = vec![
            GroqWord {
                word: "world".to_string(),
                start: 0.6,
                end: 1.0,
            },
            GroqWord {
                word: "how".to_string(),
                start: 1.1,
                end: 1.3,
            },
        ];
        // No offset advance — simulate overlap.
        let accepted2 = tracker.filter_words(&words2);
        assert_eq!(accepted2.len(), 1);
        assert_eq!(accepted2[0].word, "how");
    }

    #[test]
    fn timestamp_dedup_with_offset() {
        let mut tracker = DeduplicationTracker::new();

        let words1 = vec![GroqWord {
            word: "Hello".to_string(),
            start: 0.0,
            end: 0.5,
        }];
        tracker.filter_words(&words1);

        // Advance offset by 1 second (first chunk was 1s of audio).
        tracker.advance_offset(1.0);

        // Second chunk: timestamps restart from 0 but offset adjusts them.
        let words2 = vec![GroqWord {
            word: "world".to_string(),
            start: 0.1,
            end: 0.5,
        }];
        let accepted = tracker.filter_words(&words2);
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].word, "world");
        // Adjusted start should be ~1.1.
        assert!((accepted[0].start - 1.1).abs() < 0.01);
    }

    #[test]
    fn text_dedup_no_previous() {
        let mut tracker = DeduplicationTracker::new();
        let result = tracker.filter_text("Hello world");
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn text_dedup_removes_overlap() {
        let mut tracker = DeduplicationTracker::new();
        tracker.filter_text("Hello world this");
        let result = tracker.filter_text("world this is a test");
        assert_eq!(result, "is a test");
    }

    #[test]
    fn text_dedup_no_overlap_found() {
        let mut tracker = DeduplicationTracker::new();
        tracker.filter_text("Hello world");
        let result = tracker.filter_text("completely different text");
        assert_eq!(result, "completely different text");
    }

    #[test]
    fn text_dedup_full_overlap() {
        let mut tracker = DeduplicationTracker::new();
        tracker.filter_text("Hello world");
        let result = tracker.filter_text("Hello world");
        // The entire new text overlaps.
        assert_eq!(result, "");
    }

    #[test]
    fn normalize_word_strips_punctuation() {
        assert_eq!(normalize_word("Hello,"), "hello");
        assert_eq!(normalize_word("world."), "world");
        assert_eq!(normalize_word("test"), "test");
    }

    #[test]
    fn words_match_exact() {
        assert!(words_match("hello", "hello"));
        assert!(words_match("Hello", "hello"));
    }

    #[test]
    fn words_match_with_punctuation() {
        assert!(words_match("hello,", "hello"));
        assert!(words_match("world.", "world"));
    }

    #[test]
    fn words_match_fuzzy() {
        // Small edit distance should still match.
        assert!(words_match("hello", "helo"));
    }

    #[test]
    fn words_dont_match_very_different() {
        assert!(!words_match("hello", "world"));
    }

    #[test]
    fn remove_overlap_basic() {
        let result = remove_overlap("the quick brown", "brown fox jumps");
        assert_eq!(result, "fox jumps");
    }

    #[test]
    fn remove_overlap_multi_word() {
        let result = remove_overlap("one two three four", "three four five six");
        assert_eq!(result, "five six");
    }

    #[test]
    fn remove_overlap_none() {
        let result = remove_overlap("hello world", "completely different");
        assert_eq!(result, "completely different");
    }
}
