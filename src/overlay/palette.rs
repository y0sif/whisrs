//! The overlay themes' colors. The pill draws with them, and the TUI takes
//! its colors from the same theme so the two match. Not behind the `overlay`
//! feature: a build without the pill still has a TUI to color.

use tracing::warn;

use crate::OverlayConfig;

/// Color palette for one overlay theme. Bytes are stored as `[A, R, G, B]`,
/// matching the pill's canvas pixel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub bg: [u8; 4],
    pub ring: [u8; 4],
    pub rec_bar: [u8; 4],
    pub trans_bar: [u8; 4],
    /// Read-aloud "speaking" bar color, a distinct hue from `rec_bar` so the
    /// user can tell dictation from read-aloud at a glance.
    pub speak_bar: [u8; 4],
    pub glow: [u8; 4],
}

impl Palette {
    /// Warm "tally light" amber on near-black slate. The fallback for an
    /// unknown theme, and the base `custom` overrides.
    pub const fn ember() -> Self {
        Self {
            bg: [235, 14, 14, 16],           // #0E0E10 @ 92%
            ring: [64, 249, 115, 22],        // #F97316 @ 25%
            rec_bar: [255, 249, 115, 22],    // #F97316
            trans_bar: [255, 240, 237, 245], // #F0EDF5
            speak_bar: [255, 52, 211, 153],  // #34D399 emerald, distinct from amber
            glow: [60, 249, 115, 22],
        }
    }

    /// Monochrome terminal palette, subdued and never distracting. The
    /// default theme.
    pub const fn carbon() -> Self {
        Self {
            bg: [235, 14, 14, 16],
            ring: [80, 58, 58, 64],          // hairline gray
            rec_bar: [255, 240, 237, 245],   // soft white
            trans_bar: [255, 156, 163, 175], // warm gray
            speak_bar: [255, 45, 212, 191],  // #2DD4BF teal, pops against the grays
            glow: [40, 240, 237, 245],
        }
    }

    /// Cool electric-blue palette with an audio-equipment vibe.
    pub const fn cyan() -> Self {
        Self {
            bg: [235, 10, 15, 20],
            ring: [64, 34, 211, 238], // #22D3EE @ 25%
            rec_bar: [255, 34, 211, 238],
            trans_bar: [255, 56, 189, 248], // #38BDF8
            speak_bar: [255, 74, 222, 128], // #4ADE80 green, distinct from cyan
            glow: [50, 34, 211, 238],
        }
    }

    /// The palette of `cfg`'s theme, with `[overlay.colors]` applied when the
    /// theme is `custom`.
    pub fn from_config(cfg: &OverlayConfig) -> Self {
        let base = match cfg.theme.as_str() {
            "carbon" => Self::carbon(),
            "cyan" => Self::cyan(),
            "ember" | "custom" => Self::ember(),
            other => {
                warn!("unknown overlay theme {other:?}, falling back to ember");
                Self::ember()
            }
        };
        if cfg.theme != "custom" {
            return base;
        }
        let Some(c) = cfg.colors.as_ref() else {
            return base;
        };
        Self {
            bg: c
                .background
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.bg),
            ring: c
                .ring
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.ring),
            rec_bar: c
                .recording
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.rec_bar),
            trans_bar: c
                .transcribing
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.trans_bar),
            speak_bar: c
                .speaking
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.speak_bar),
            glow: c
                .glow
                .as_deref()
                .and_then(crate::parse_hex_color)
                .unwrap_or(base.glow),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The palette an `[overlay]` section written as `toml` picks.
    fn palette(toml: &str) -> Palette {
        let cfg: OverlayConfig = toml::from_str(toml).expect("fixture parses");
        Palette::from_config(&cfg)
    }

    /// `[R, G, B]` of an `[A, R, G, B]` color, opaque.
    fn opaque_rgb(argb: [u8; 4]) -> [u8; 3] {
        assert_eq!(argb[0], 255, "{argb:?} is not opaque");
        [argb[1], argb[2], argb[3]]
    }

    /// The colors the TUI reads, by theme name: the recording bar (its
    /// accent) and the speaking bar (its "ok"). This module is outside the
    /// `overlay` feature, so the test runs in a build without it too.
    #[test]
    fn each_theme_has_its_recording_and_speaking_colors() {
        for (theme, recording, speaking) in [
            ("carbon", [0xF0, 0xED, 0xF5], [0x2D, 0xD4, 0xBF]),
            ("ember", [0xF9, 0x73, 0x16], [0x34, 0xD3, 0x99]),
            ("cyan", [0x22, 0xD3, 0xEE], [0x4A, 0xDE, 0x80]),
        ] {
            let p = palette(&format!("theme = \"{theme}\""));
            assert_eq!(opaque_rgb(p.rec_bar), recording, "{theme}");
            assert_eq!(opaque_rgb(p.speak_bar), speaking, "{theme}");
        }
        assert_eq!(palette("theme = \"carbon\""), Palette::carbon());
        assert_eq!(palette("theme = \"ember\""), Palette::ember());
        assert_eq!(palette("theme = \"cyan\""), Palette::cyan());
    }

    /// Carbon is the default, both for a missing `[overlay]` section and
    /// for one without a `theme`.
    #[test]
    fn the_default_theme_is_carbon() {
        assert_eq!(
            Palette::from_config(&OverlayConfig::default()),
            Palette::carbon()
        );
        assert_eq!(palette(""), Palette::carbon());
    }

    #[test]
    fn an_unknown_theme_falls_back_to_ember() {
        assert_eq!(palette("theme = \"neon\""), Palette::ember());
    }

    /// `custom` takes each color it sets and keeps ember's for the rest; a
    /// color that does not parse keeps ember's too. `[overlay.colors]` under
    /// any other theme is ignored.
    #[test]
    fn custom_overrides_ember_one_color_at_a_time() {
        let custom = palette(
            "theme = \"custom\"\n[colors]\nrecording = \"#112233\"\nspeaking = \"#456\"\n\
             ring = \"not a color\"\n",
        );
        assert_eq!(custom.rec_bar, [255, 0x11, 0x22, 0x33]);
        assert_eq!(custom.speak_bar, [255, 0x44, 0x55, 0x66]);
        let ember = Palette::ember();
        assert_eq!(custom.ring, ember.ring);
        assert_eq!(custom.bg, ember.bg);
        assert_eq!(custom.trans_bar, ember.trans_bar);
        assert_eq!(custom.glow, ember.glow);

        assert_eq!(palette("theme = \"custom\""), Palette::ember());
        assert_eq!(
            palette("theme = \"cyan\"\n[colors]\nrecording = \"#112233\"\n"),
            Palette::cyan()
        );
    }
}
