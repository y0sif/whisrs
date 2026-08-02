use tracing::warn;

/// Truncate a string to at most `max_bytes` bytes, respecting UTF-8 char
/// boundaries. Appends "..." if truncated.
pub(crate) fn truncate_preview(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max_bytes)
        .last()
        .unwrap_or(0);
    format!("{}...", &text[..end])
}

pub(crate) fn send_notification(summary: &str, body: &str) {
    let summary = summary.to_string();
    let body = body.to_string();
    // Run on a blocking thread to avoid D-Bus block_on conflict with ksni tray.
    std::thread::spawn(move || {
        if let Err(e) = notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .appname("whisrs")
            .timeout(notify_rust::Timeout::Milliseconds(2000))
            .show()
        {
            warn!("failed to send notification: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii_short() {
        assert_eq!(truncate_preview("hello", 77), "hello");
    }

    #[test]
    fn truncate_ascii_exact() {
        let text = "a".repeat(77);
        assert_eq!(truncate_preview(&text, 77), text);
    }

    #[test]
    fn truncate_ascii_long() {
        let text = "a".repeat(100);
        let expected = format!("{}...", "a".repeat(77));
        assert_eq!(truncate_preview(&text, 77), expected);
    }

    #[test]
    fn truncate_cyrillic() {
        // Cyrillic chars are 2 bytes each. 40 Cyrillic chars = 80 bytes > 77.
        let text = "а".repeat(40); // 80 bytes
        let preview = truncate_preview(&text, 77);
        assert!(preview.ends_with("..."));
        // Should truncate to 38 chars (76 bytes) + "..." since 39 chars = 78 bytes > 77.
        assert_eq!(preview, format!("{}...", "а".repeat(38)));
    }

    #[test]
    fn truncate_cyrillic_issue_repro() {
        // Exact text from issue #4.
        let text = "Вот это рутина, блять, вы дешевые щели, блять.";
        let preview = truncate_preview(text, 77);
        // Must not panic and must be valid UTF-8.
        assert!(preview.is_char_boundary(0));
        assert!(!preview.is_empty());
    }

    #[test]
    fn truncate_cjk() {
        // CJK chars are 3 bytes each. 27 CJK chars = 81 bytes > 77.
        let text = "字".repeat(27);
        let preview = truncate_preview(&text, 77);
        assert!(preview.ends_with("..."));
        // 25 chars = 75 bytes (fits), 26 chars = 78 bytes (too big).
        assert_eq!(preview, format!("{}...", "字".repeat(25)));
    }

    #[test]
    fn truncate_arabic() {
        // Arabic chars are 2 bytes each. 40 Arabic chars = 80 bytes > 77.
        let text = "ش".repeat(40);
        let preview = truncate_preview(&text, 77);
        assert!(preview.ends_with("..."));
        assert_eq!(preview, format!("{}...", "ش".repeat(38)));
    }

    #[test]
    fn truncate_emoji() {
        // Emoji are 4 bytes each. 20 emoji = 80 bytes > 77.
        let text = "😀".repeat(20);
        let preview = truncate_preview(&text, 77);
        assert!(preview.ends_with("..."));
        // 19 emoji = 76 bytes (fits).
        assert_eq!(preview, format!("{}...", "😀".repeat(19)));
    }

    #[test]
    fn truncate_mixed_scripts() {
        // Mix of ASCII (1 byte), Cyrillic (2 bytes), CJK (3 bytes).
        let text = "Hello Мир 世界! This is a test of mixed script truncation that is long enough";
        let preview = truncate_preview(text, 77);
        assert!(preview.ends_with("..."));
        // Verify it doesn't panic and produces valid UTF-8.
        assert!(preview.len() <= 80); // 77 + "..."
    }

    #[test]
    fn truncate_empty() {
        assert_eq!(truncate_preview("", 77), "");
    }
}
