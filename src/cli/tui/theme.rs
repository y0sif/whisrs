//! The TUI's colour roles. Screens name a role by its job (accent, ok,
//! muted, faint, warn, error), never a colour, and the role decides how to
//! say it on this terminal.
//!
//! Accent and ok come from the effective `[overlay] theme`, so the pill and
//! the TUI match: carbon (the default), ember, cyan, or custom
//! (`[overlay.colors] recording` and `speaking`, ember's for what it leaves
//! unset, as the pill does). Truecolor terminals get the theme's colours;
//! the rest a fixed 16-colour map; `NO_COLOR` none at all. Nothing paints a
//! background, so on a light terminal remapping the roles is enough.

use ratatui::style::{Color, Modifier, Style};
use terminal_colorsaurus::{QueryOptions, ThemeMode};
use whisrs::overlay::palette::Palette;
use whisrs::OverlayConfig;

use super::Env;

/// How much foreground survives in `muted` (help text, inherited values).
const MUTED_ALPHA: f32 = 0.62;

/// How much foreground survives in `faint` (the frame, rules).
const FAINT_ALPHA: f32 = 0.34;

/// The lightness a saturated role colour keeps on a light background.
const LIGHT_MAX_LIGHTNESS: f32 = 0.42;

/// Below this chroma (max minus min channel, 0 to 1) a colour counts as a
/// neutral and flips its lightness on a light background instead.
const NEUTRAL_CHROMA: f32 = 0.25;

/// A 24-bit colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// The `[A, R, G, B]` of a pill palette colour, alpha dropped.
    pub fn from_argb(argb: [u8; 4]) -> Rgb {
        Rgb(argb[1], argb[2], argb[3])
    }

    fn color(self) -> Color {
        Color::Rgb(self.0, self.1, self.2)
    }
}

/// How many colours the terminal shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// `COLORTERM` is `truecolor` or `24bit`.
    Truecolor,
    /// Anything else: the 16 colours the user's terminal theme sets.
    Ansi16,
    /// `NO_COLOR` is set and not empty.
    NoColor,
}

impl Depth {
    pub fn detect(env: &Env) -> Depth {
        if env.get("NO_COLOR").is_some_and(|value| !value.is_empty()) {
            Depth::NoColor
        } else if matches!(env.get("COLORTERM"), Some("truecolor" | "24bit")) {
            Depth::Truecolor
        } else {
            Depth::Ansi16
        }
    }
}

/// What the TUI is drawn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ground {
    pub light: bool,
    /// The terminal's foreground and background, when it told us.
    pub colors: Option<(Rgb, Rgb)>,
}

impl Ground {
    /// Ask the terminal for its foreground and background (OSC 10 and 11).
    /// Runs before raw mode and the input reader, so the answer cannot be
    /// read as keystrokes. The wait is the crate's (1 s): a terminal that
    /// does not answer is caught at once by the DA1 fence sent after the
    /// query. One that does not answer falls back to `COLORFGBG`, which
    /// kitty and foot do not set, then to dark.
    ///
    /// Not asked over SSH ([`remote`]): there the reply can come after the
    /// wait, into the TUI, as keys.
    pub fn probe(env: &Env) -> Ground {
        if remote(env) {
            return Ground {
                light: colorfgbg_is_light(env).unwrap_or(false),
                colors: None,
            };
        }
        match terminal_colorsaurus::color_palette(QueryOptions::default()) {
            Ok(palette) => {
                let rgb = |c: &terminal_colorsaurus::Color| {
                    let (r, g, b) = c.scale_to_8bit();
                    Rgb(r, g, b)
                };
                Ground {
                    light: palette.theme_mode() == ThemeMode::Light,
                    colors: Some((rgb(&palette.foreground), rgb(&palette.background))),
                }
            }
            Err(_) => Ground {
                light: colorfgbg_is_light(env).unwrap_or(false),
                colors: None,
            },
        }
    }
}

/// Whether the TUI runs over SSH, where a terminal's answer can be slow.
pub fn remote(env: &Env) -> bool {
    ["SSH_TTY", "SSH_CONNECTION"]
        .into_iter()
        .any(|key| env.get(key).is_some_and(|value| !value.is_empty()))
}

/// Whether `COLORFGBG` (`fg;bg`, or `fg;default;bg`) names a light
/// background: ANSI 7 or one of the bright colours but bright black.
fn colorfgbg_is_light(env: &Env) -> Option<bool> {
    let bg: u8 = env.get("COLORFGBG")?.rsplit(';').next()?.parse().ok()?;
    Some(bg == 7 || (9..=15).contains(&bg))
}

/// The colour roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// The gutter and the focused value.
    pub accent: Style,
    /// The daemon is up, a save went through.
    pub ok: Style,
    /// Help text and labels away from focus.
    pub muted: Style,
    /// The frame and separators.
    pub faint: Style,
    pub warn: Style,
    pub error: Style,
    /// False under `NO_COLOR`, where an error also says `error:`.
    pub color: bool,
    /// How many colours there are, for a swatch.
    pub depth: Depth,
}

impl Theme {
    /// The roles for `overlay`'s theme at `depth`, on `ground`.
    pub fn new(overlay: &OverlayConfig, depth: Depth, ground: Ground) -> Theme {
        let palette = Palette::from_config(overlay);
        let (accent, ok) = match depth {
            Depth::NoColor => {
                return Theme {
                    accent: Style::new().add_modifier(Modifier::BOLD),
                    ok: Style::new(),
                    muted: Style::new().add_modifier(Modifier::DIM),
                    faint: Style::new().add_modifier(Modifier::DIM),
                    warn: Style::new(),
                    error: Style::new().add_modifier(Modifier::BOLD),
                    color: false,
                    depth,
                };
            }
            Depth::Truecolor => {
                let role = |argb| {
                    let rgb = Rgb::from_argb(argb);
                    let rgb = if ground.light { for_light(rgb) } else { rgb };
                    Style::new().fg(rgb.color())
                };
                (role(palette.rec_bar), role(palette.speak_bar))
            }
            Depth::Ansi16 => ansi16(overlay, ground.light),
        };
        let (muted, faint) = match (depth, ground.colors) {
            (Depth::Truecolor, Some((fg, bg))) => (
                Style::new().fg(blend(fg, bg, MUTED_ALPHA).color()),
                Style::new().fg(blend(fg, bg, FAINT_ALPHA).color()),
            ),
            _ => (
                Style::new().add_modifier(Modifier::DIM),
                Style::new().fg(Color::DarkGray),
            ),
        };
        Theme {
            accent,
            ok,
            muted,
            faint,
            warn: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red),
            color: true,
            depth,
        }
    }

    /// A swatch of `rgb`: the colour itself, the nearest of the 16, or
    /// `None` with no colour to show it in.
    pub fn swatch(&self, rgb: Rgb) -> Option<Style> {
        match self.depth {
            Depth::Truecolor => Some(Style::new().fg(rgb.color())),
            Depth::Ansi16 => Some(Style::new().fg(nearest16(rgb))),
            Depth::NoColor => None,
        }
    }
}

/// Accent and ok in the 16 colours: carbon white (bold, so it stands out
/// from body text) and cyan, ember yellow and green, cyan cyan and green,
/// custom the nearest of the 16 to each colour it sets. On a light
/// background carbon's white turns black; the others are the user's own
/// palette entries, which a light theme already keeps readable.
fn ansi16(overlay: &OverlayConfig, light: bool) -> (Style, Style) {
    const EMBER: (Color, Color) = (Color::Yellow, Color::Green);
    let (accent, ok) = match overlay.theme.as_str() {
        "carbon" => {
            let white = if light { Color::Black } else { Color::White };
            return (
                Style::new().fg(white).add_modifier(Modifier::BOLD),
                Style::new().fg(Color::Cyan),
            );
        }
        "cyan" => (Color::Cyan, Color::Green),
        "custom" => {
            let colors = overlay.colors.as_ref();
            let nearest_of = |hex: Option<&String>, fallback| {
                hex.and_then(|hex| whisrs::parse_hex_color(hex))
                    .map(|argb| nearest16(Rgb::from_argb(argb)))
                    .unwrap_or(fallback)
            };
            (
                nearest_of(colors.and_then(|c| c.recording.as_ref()), EMBER.0),
                nearest_of(colors.and_then(|c| c.speaking.as_ref()), EMBER.1),
            )
        }
        // Ember, and the unknown names the pill also draws as ember.
        _ => EMBER,
    };
    (Style::new().fg(accent), Style::new().fg(ok))
}

/// The 16 ANSI colours at xterm's default values.
const ANSI16: [(Color, Rgb); 16] = [
    (Color::Black, Rgb(0, 0, 0)),
    (Color::Red, Rgb(205, 0, 0)),
    (Color::Green, Rgb(0, 205, 0)),
    (Color::Yellow, Rgb(205, 205, 0)),
    (Color::Blue, Rgb(0, 0, 238)),
    (Color::Magenta, Rgb(205, 0, 205)),
    (Color::Cyan, Rgb(0, 205, 205)),
    (Color::Gray, Rgb(229, 229, 229)),
    (Color::DarkGray, Rgb(127, 127, 127)),
    (Color::LightRed, Rgb(255, 0, 0)),
    (Color::LightGreen, Rgb(0, 255, 0)),
    (Color::LightYellow, Rgb(255, 255, 0)),
    (Color::LightBlue, Rgb(92, 92, 255)),
    (Color::LightMagenta, Rgb(255, 0, 255)),
    (Color::LightCyan, Rgb(0, 255, 255)),
    (Color::White, Rgb(255, 255, 255)),
];

/// The ANSI colour nearest `rgb`, by squared distance.
fn nearest16(rgb: Rgb) -> Color {
    let distance = |c: Rgb| {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(c.0, rgb.0) + d(c.1, rgb.1) + d(c.2, rgb.2)
    };
    ANSI16
        .iter()
        .min_by_key(|(_, c)| distance(*c))
        .map(|(color, _)| *color)
        .expect("ANSI16 is not empty")
}

/// `fg * alpha + bg * (1 - alpha)` per channel: this theme's text faded
/// toward this theme's background.
fn blend(fg: Rgb, bg: Rgb, alpha: f32) -> Rgb {
    let channel = |f: u8, b: u8| {
        (f32::from(f) * alpha + f32::from(b) * (1.0 - alpha))
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Rgb(
        channel(fg.0, bg.0),
        channel(fg.1, bg.1),
        channel(fg.2, bg.2),
    )
}

/// A role colour for a light background: a neutral (carbon's soft white)
/// flips its lightness, a saturated one is capped at
/// [`LIGHT_MAX_LIGHTNESS`] so it stays readable on paper.
fn for_light(rgb: Rgb) -> Rgb {
    let (h, s, l, chroma) = to_hsl(rgb);
    let l = if chroma < NEUTRAL_CHROMA {
        1.0 - l
    } else {
        l.min(LIGHT_MAX_LIGHTNESS)
    };
    from_hsl(h, s, l)
}

/// Hue (degrees), saturation and lightness (0 to 1), and the chroma.
fn to_hsl(Rgb(r, g, b): Rgb) -> (f32, f32, f32, f32) {
    let (r, g, b) = (
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
    );
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let d = max - min;
    if d == 0.0 {
        return (0.0, 0.0, l, 0.0);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, s, l, d)
}

fn from_hsl(h: f32, s: f32, l: f32) -> Rgb {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (h / 60.0) as u8 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |u: f32| ((u + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Rgb(to(r), to(g), to(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(toml: &str) -> OverlayConfig {
        toml::from_str(toml).expect("fixture parses")
    }

    const DARK: Ground = Ground {
        light: false,
        colors: Some((Rgb(220, 220, 220), Rgb(20, 20, 24))),
    };
    const LIGHT: Ground = Ground {
        light: true,
        colors: Some((Rgb(30, 30, 30), Rgb(250, 250, 245))),
    };

    /// The four themes, as `[overlay]` sections, with their truecolor
    /// accent and ok on a dark background. Custom sets its accent and
    /// leaves `speaking` to ember.
    const THEMES: [(&str, &str, Rgb, Rgb); 4] = [
        ("carbon", "", Rgb(0xF0, 0xED, 0xF5), Rgb(0x2D, 0xD4, 0xBF)),
        (
            "ember",
            "theme = \"ember\"",
            Rgb(0xF9, 0x73, 0x16),
            Rgb(0x34, 0xD3, 0x99),
        ),
        (
            "cyan",
            "theme = \"cyan\"",
            Rgb(0x22, 0xD3, 0xEE),
            Rgb(0x4A, 0xDE, 0x80),
        ),
        (
            "custom",
            "theme = \"custom\"\n[colors]\nrecording = \"#8B5CF6\"\n",
            Rgb(0x8B, 0x5C, 0xF6),
            Rgb(0x34, 0xD3, 0x99),
        ),
    ];

    fn lightness(style: Style) -> f32 {
        let Some(Color::Rgb(r, g, b)) = style.fg else {
            panic!("not a truecolor role: {style:?}");
        };
        to_hsl(Rgb(r, g, b)).2
    }

    #[test]
    fn dark_truecolor_takes_the_theme_colours() {
        for (name, toml, accent, ok) in THEMES {
            let theme = Theme::new(&overlay(toml), Depth::Truecolor, DARK);
            assert_eq!(theme.accent.fg, Some(accent.color()), "{name}");
            assert_eq!(theme.ok.fg, Some(ok.color()), "{name}");
            assert!(theme.color);
        }
    }

    /// Custom keeps ember's colours for what `[overlay.colors]` leaves
    /// unset, as the pill does.
    #[test]
    fn custom_falls_back_to_ember() {
        let bare = Theme::new(&overlay("theme = \"custom\""), Depth::Truecolor, DARK);
        let ember = Theme::new(&overlay("theme = \"ember\""), Depth::Truecolor, DARK);
        assert_eq!(bare, ember);
        let bare = Theme::new(&overlay("theme = \"custom\""), Depth::Ansi16, DARK);
        let ember = Theme::new(&overlay("theme = \"ember\""), Depth::Ansi16, DARK);
        assert_eq!(bare, ember);
    }

    /// On a light background carbon's soft white turns to ink and the
    /// saturated accents deepen; every one stays readable on paper.
    #[test]
    fn light_truecolor_remaps_each_role() {
        for (name, toml, _, _) in THEMES {
            let theme = Theme::new(&overlay(toml), Depth::Truecolor, LIGHT);
            for (role, style) in [("accent", theme.accent), ("ok", theme.ok)] {
                assert!(
                    lightness(style) <= LIGHT_MAX_LIGHTNESS + 0.01,
                    "{name} {role}: {style:?}"
                );
            }
        }
        let carbon = Theme::new(&OverlayConfig::default(), Depth::Truecolor, LIGHT);
        assert!(lightness(carbon.accent) < 0.1, "{:?}", carbon.accent);
    }

    #[test]
    fn muted_and_faint_blend_the_terminal_colours() {
        let theme = Theme::new(&OverlayConfig::default(), Depth::Truecolor, DARK);
        assert_eq!(theme.muted.fg, Some(Color::Rgb(144, 144, 146)));
        assert_eq!(theme.faint.fg, Some(Color::Rgb(88, 88, 91)));
        // A terminal that did not say gets the portable spellings.
        let unknown = Ground {
            light: false,
            colors: None,
        };
        let theme = Theme::new(&OverlayConfig::default(), Depth::Truecolor, unknown);
        assert!(theme.muted.add_modifier.contains(Modifier::DIM));
        assert_eq!(theme.faint.fg, Some(Color::DarkGray));
    }

    #[test]
    fn sixteen_colours_use_the_fixed_map() {
        let expected = [
            ("carbon", Color::White, Color::Cyan),
            ("ember", Color::Yellow, Color::Green),
            ("cyan", Color::Cyan, Color::Green),
            // #8B5CF6 violet is nearest the bright blue; speaking is ember's.
            ("custom", Color::LightBlue, Color::Green),
        ];
        for ((name, toml, _, _), (_, accent, ok)) in THEMES.into_iter().zip(expected) {
            let theme = Theme::new(&overlay(toml), Depth::Ansi16, DARK);
            assert_eq!(theme.accent.fg, Some(accent), "{name}");
            assert_eq!(theme.ok.fg, Some(ok), "{name}");
            // No blends without truecolor, even with the colours known.
            assert!(theme.muted.add_modifier.contains(Modifier::DIM), "{name}");
            assert_eq!(theme.faint.fg, Some(Color::DarkGray), "{name}");
        }
        let carbon = Theme::new(&OverlayConfig::default(), Depth::Ansi16, DARK);
        assert!(carbon.accent.add_modifier.contains(Modifier::BOLD));
        let carbon = Theme::new(&OverlayConfig::default(), Depth::Ansi16, LIGHT);
        assert_eq!(carbon.accent.fg, Some(Color::Black));
    }

    #[test]
    fn no_color_has_no_colour_at_all() {
        for (name, toml, _, _) in THEMES {
            for ground in [DARK, LIGHT] {
                let theme = Theme::new(&overlay(toml), Depth::NoColor, ground);
                for style in [
                    theme.accent,
                    theme.ok,
                    theme.muted,
                    theme.faint,
                    theme.warn,
                    theme.error,
                ] {
                    assert_eq!((style.fg, style.bg), (None, None), "{name}");
                }
                assert!(theme.accent.add_modifier.contains(Modifier::BOLD));
                assert!(theme.muted.add_modifier.contains(Modifier::DIM));
                assert!(theme.error.add_modifier.contains(Modifier::BOLD));
                assert!(!theme.color);
            }
        }
    }

    #[test]
    fn depth_follows_no_color_then_colorterm() {
        let depth = |pairs: &[(&str, &str)]| Depth::detect(&Env::from_pairs(pairs));
        assert_eq!(depth(&[("COLORTERM", "truecolor")]), Depth::Truecolor);
        assert_eq!(depth(&[("COLORTERM", "24bit")]), Depth::Truecolor);
        assert_eq!(depth(&[]), Depth::Ansi16);
        assert_eq!(depth(&[("COLORTERM", "yes")]), Depth::Ansi16);
        assert_eq!(
            depth(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")]),
            Depth::NoColor
        );
        // An empty NO_COLOR does not count.
        assert_eq!(depth(&[("NO_COLOR", "")]), Depth::Ansi16);
    }

    /// Over SSH the terminal is not asked: the ground is dark, or what
    /// `COLORFGBG` says.
    #[test]
    fn the_probe_is_skipped_over_ssh() {
        let ssh = Env::from_pairs(&[("SSH_TTY", "/dev/pts/3")]);
        assert!(remote(&ssh));
        assert_eq!(Ground::probe(&ssh), Ground::default());
        let ssh_light = Env::from_pairs(&[
            ("SSH_CONNECTION", "10.0.0.2 51234 10.0.0.1 22"),
            ("COLORFGBG", "0;15"),
        ]);
        assert!(remote(&ssh_light));
        assert_eq!(
            Ground::probe(&ssh_light),
            Ground {
                light: true,
                colors: None
            }
        );
        assert!(!remote(&Env::from_pairs(&[("SSH_TTY", "")])));
        assert!(!remote(&Env::default()));
    }

    #[test]
    fn colorfgbg_names_the_background() {
        let light = |value: &str| colorfgbg_is_light(&Env::from_pairs(&[("COLORFGBG", value)]));
        assert_eq!(light("15;0"), Some(false));
        assert_eq!(light("0;15"), Some(true));
        assert_eq!(light("0;default;7"), Some(true));
        assert_eq!(light("7;8"), Some(false));
        assert_eq!(light("nonsense"), None);
        assert_eq!(colorfgbg_is_light(&Env::default()), None);
    }

    #[test]
    fn hsl_round_trips() {
        for rgb in [
            Rgb(255, 140, 0),
            Rgb(40, 170, 50),
            Rgb(255, 0, 200),
            Rgb(12, 34, 56),
        ] {
            let (h, s, l, _) = to_hsl(rgb);
            assert_eq!(from_hsl(h, s, l), rgb);
        }
    }
}
