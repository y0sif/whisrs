//! The characters the TUI draws with: a Unicode set, and an ASCII one for a
//! terminal that is not in a UTF-8 locale.
//!
//! Every glyph in both sets is exactly one `char` and one column wide, so
//! the layout can count columns by counting characters. No emoji, nothing
//! that renders double-width; warnings use `!` in both sets.

use ratatui::symbols::border;

use super::Env;

/// The TUI's characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    /// The screen's frame (rounded in Unicode).
    pub frame: border::Set<'static>,
    /// A dialog's frame, the only thick box.
    pub dialog: border::Set<'static>,
    /// Marks the focused row.
    pub gutter: &'static str,
    /// The header waveform's three heights, low to high.
    pub wave: [&'static str; 3],
    /// The daemon is up.
    pub up: &'static str,
    /// The daemon is down, or not known yet.
    pub down: &'static str,
    /// Either side of a choice on the focused row.
    pub left: &'static str,
    pub right: &'static str,
    /// Between two items on one line.
    pub separator: &'static str,
    /// Ends a value cut to fit.
    pub ellipsis: &'static str,
    /// One typed character of a secret.
    pub bullet: &'static str,
    /// Leads a warning line.
    pub warn: &'static str,
    /// Leads a picker's filter line.
    pub prompt: &'static str,
    /// Marks the value a picker's row has now.
    pub current: &'static str,
    /// A percent bar's filled and empty cells.
    pub bar_on: &'static str,
    pub bar_off: &'static str,
    /// A colour's swatch, drawn twice in that colour.
    pub swatch: &'static str,
    /// The arrow keys, in key hints.
    pub key_up: &'static str,
    pub key_down: &'static str,
    /// The ASCII set: text with no one-character ASCII stand-in (`±`) is
    /// spelled out instead.
    pub ascii: bool,
}

impl Glyphs {
    pub const UNICODE: Glyphs = Glyphs {
        frame: border::ROUNDED,
        dialog: border::THICK,
        gutter: "\u{258C}",                         // ▌ LEFT HALF BLOCK
        wave: ["\u{2581}", "\u{2583}", "\u{2585}"], // ▁ ▃ ▅
        up: "\u{25CF}",                             // ● BLACK CIRCLE
        down: "\u{25CB}",                           // ○ WHITE CIRCLE
        left: "\u{25C2}",                           // ◂ BLACK LEFT-POINTING SMALL TRIANGLE
        right: "\u{25B8}",                          // ▸ BLACK RIGHT-POINTING SMALL TRIANGLE
        separator: "\u{00B7}",                      // · MIDDLE DOT
        ellipsis: "\u{2026}",                       // … HORIZONTAL ELLIPSIS
        bullet: "\u{2022}",                         // • BULLET
        warn: "!",
        prompt: "\u{203A}",   // › SINGLE RIGHT-POINTING ANGLE QUOTATION MARK
        current: "\u{25CF}",  // ● BLACK CIRCLE
        bar_on: "\u{25B0}",   // ▰ BLACK PARALLELOGRAM
        bar_off: "\u{25B1}",  // ▱ WHITE PARALLELOGRAM
        swatch: "\u{2588}",   // █ FULL BLOCK
        key_up: "\u{2191}",   // ↑ UPWARDS ARROW
        key_down: "\u{2193}", // ↓ DOWNWARDS ARROW
        ascii: false,
    };

    pub const ASCII: Glyphs = Glyphs {
        frame: border::Set {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            vertical_left: "|",
            vertical_right: "|",
            horizontal_top: "-",
            horizontal_bottom: "-",
        },
        dialog: border::Set {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            vertical_left: "|",
            vertical_right: "|",
            horizontal_top: "=",
            horizontal_bottom: "=",
        },
        gutter: ">",
        wave: [".", ":", "|"],
        up: "*",
        down: "o",
        left: "<",
        right: ">",
        separator: "|",
        ellipsis: "~",
        bullet: "*",
        warn: "!",
        prompt: ">",
        current: "*",
        bar_on: "#",
        bar_off: "-",
        swatch: "#",
        key_up: "^",
        key_down: "v",
        ascii: true,
    };

    /// Plus or minus, before a step in a key hint.
    pub fn plus_minus(&self) -> &'static str {
        if self.ascii {
            "+/-"
        } else {
            "\u{00B1}"
        }
    }

    /// The set `env`'s locale can show.
    pub fn detect(env: &Env) -> Glyphs {
        if wants_ascii(env) {
            Glyphs::ASCII
        } else {
            Glyphs::UNICODE
        }
    }
}

/// Whether `env` gets the ASCII set: the first of `LC_ALL`, `LC_CTYPE` and
/// `LANG` that is set and not empty decides (POSIX precedence, so `LC_ALL=C`
/// wins over a UTF-8 `LANG`), and none of them set is the `C` locale.
fn wants_ascii(env: &Env) -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|key| env.get(key).filter(|value| !value.is_empty()))
        .is_none_or(|locale| {
            let locale = locale.to_ascii_lowercase();
            !(locale.contains("utf8") || locale.contains("utf-8"))
        })
}

#[cfg(test)]
mod tests {
    use ratatui::text::Span;

    use super::*;

    /// Every glyph of one set, so a test cannot pass by forgetting a field.
    fn all(set: &Glyphs) -> Vec<&'static str> {
        let Glyphs {
            frame,
            dialog,
            gutter,
            wave,
            up,
            down,
            left,
            right,
            separator,
            ellipsis,
            bullet,
            warn,
            prompt,
            current,
            bar_on,
            bar_off,
            swatch,
            key_up,
            key_down,
            ascii: _,
        } = *set;
        let mut glyphs = vec![
            gutter, up, down, left, right, separator, ellipsis, bullet, warn, prompt, current,
            bar_on, bar_off, swatch, key_up, key_down,
        ];
        glyphs.extend(wave);
        for border in [frame, dialog] {
            glyphs.extend([
                border.top_left,
                border.top_right,
                border.bottom_left,
                border.bottom_right,
                border.vertical_left,
                border.vertical_right,
                border.horizontal_top,
                border.horizontal_bottom,
            ]);
        }
        glyphs
    }

    #[test]
    fn every_glyph_is_one_char_and_one_column() {
        for (name, set) in [("unicode", Glyphs::UNICODE), ("ascii", Glyphs::ASCII)] {
            for glyph in all(&set) {
                assert_eq!(glyph.chars().count(), 1, "{name} glyph {glyph:?}");
                // The width the renderer spends on it.
                assert_eq!(Span::raw(glyph).width(), 1, "{name} glyph {glyph:?}");
            }
        }
    }

    #[test]
    fn the_ascii_set_is_ascii() {
        for glyph in all(&Glyphs::ASCII) {
            assert!(glyph.is_ascii(), "{glyph:?} is not ASCII");
        }
        assert!(Glyphs::ASCII.plus_minus().is_ascii());
        assert_eq!(Glyphs::UNICODE.plus_minus(), "\u{00B1}");
    }

    #[test]
    fn a_utf8_locale_gets_unicode_and_anything_else_ascii() {
        let detect = |pairs: &[(&str, &str)]| Glyphs::detect(&Env::from_pairs(pairs));
        assert_eq!(detect(&[("LANG", "en_US.UTF-8")]), Glyphs::UNICODE);
        assert_eq!(detect(&[("LC_CTYPE", "C.utf8")]), Glyphs::UNICODE);
        assert_eq!(detect(&[]), Glyphs::ASCII);
        assert_eq!(detect(&[("LANG", "C")]), Glyphs::ASCII);
        assert_eq!(detect(&[("LANG", "en_US.ISO-8859-1")]), Glyphs::ASCII);
        // The first one set decides, an empty one is not set.
        assert_eq!(
            detect(&[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")]),
            Glyphs::ASCII
        );
        assert_eq!(
            detect(&[("LC_ALL", ""), ("LANG", "en_US.UTF-8")]),
            Glyphs::UNICODE
        );
    }
}
