//! XKB reverse lookup table: char → (Keycode, Modifiers).

use crate::{KeyMapping, KeyTap};
use std::collections::HashMap;
use std::process::Command;

// Logging via the `log` crate (only active with `logging` feature),
// matching the conditional shim used in `keyboard.rs`.
#[cfg(feature = "logging")]
use log::debug;
#[cfg(not(feature = "logging"))]
macro_rules! debug {
    ($($arg:tt)*) => {};
}

#[derive(Debug, Clone)]
pub struct KeyboardLayout {
    pub layout: String,
    pub variant: String,
}

impl KeyboardLayout {
    pub fn detect() -> Self {
        // Order matters: compositor IPC and X11 reflect the *currently active* session
        // layout (honoring runtime switches via xkb-switch and friends), so they win when
        // reachable. localectl is the systemd source-of-truth fallback for cases where
        // X env vars aren't visible (e.g. running as a systemd --user service, issue #39).
        // Env vars come last as an explicit user override escape hatch.
        if let Some(kl) = Self::from_hyprland() {
            return kl;
        }
        if let Some(kl) = Self::from_sway() {
            return kl;
        }
        if let Some(kl) = Self::from_x11() {
            return kl;
        }
        if let Some(kl) = Self::from_localectl() {
            return kl;
        }
        if let Some(kl) = Self::from_env() {
            return kl;
        }
        Self {
            layout: String::new(),
            variant: String::new(),
        }
    }

    fn from_hyprland() -> Option<Self> {
        let output = Command::new("hyprctl")
            .args(["devices", "-j"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
        let keyboards = json.get("keyboards")?.as_array()?;
        let kb = keyboards
            .iter()
            .find(|k| {
                let name = k.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let layout = k.get("layout").and_then(|l| l.as_str()).unwrap_or("");
                !layout.is_empty() && (name.contains("translated") || name.contains("at-"))
            })
            .or_else(|| {
                keyboards.iter().find(|k| {
                    let layout = k.get("layout").and_then(|l| l.as_str()).unwrap_or("");
                    !layout.is_empty()
                })
            })?;
        let layout = kb.get("layout")?.as_str()?.to_string();
        let variant = kb
            .get("variant")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Some(Self { layout, variant })
    }

    /// Query Sway for the active keyboard layout via `swaymsg -t get_inputs`.
    ///
    /// Sway's `xkb_layout_names` array exposes *display* names like
    /// `"German"` or `"English (US)"`, not the XKB layout codes (`"de"`,
    /// `"us"`) that `xkbcommon` understands. Feeding a display name into
    /// `Keymap::new_from_names` causes xkbcommon to silently fall back to
    /// the compile-time default layout — which in turn would
    /// short-circuit the env-var / `/etc/default/keyboard` fallbacks that
    /// can produce the *correct* code.
    ///
    /// We therefore confirm Sway is reachable (so the function is not
    /// dead code) but always return `None`, letting the caller fall
    /// through to the next fallback. If a real display-name → XKB-code
    /// lookup table is added later, this is the place to wire it up.
    fn from_sway() -> Option<Self> {
        let output = Command::new("swaymsg")
            .args(["-t", "get_inputs", "--raw"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let inputs: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).ok()?;

        // Confirm there is at least one keyboard input with an active
        // layout — guards against running this on an X11 session where
        // swaymsg may exist but not return useful data.
        let kb = inputs.iter().find(|i| {
            i.get("type").and_then(|t| t.as_str()) == Some("keyboard")
                && i.get("xkb_active_layout_name").is_some()
        })?;

        // Confirm the layout-name array is well-formed; we don't use
        // the value because it's a display name, not an XKB code.
        let layout_names = kb.get("xkb_layout_names")?.as_array()?;
        let active_idx = kb
            .get("xkb_active_layout_index")
            .and_then(|i| i.as_u64())
            .unwrap_or(0) as usize;
        let _display_name = layout_names.get(active_idx)?.as_str()?;

        // Intentionally fall through — see the doc comment above.
        None
    }

    fn from_x11() -> Option<Self> {
        if std::env::var_os("DISPLAY").is_none() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return None;
        }

        let output = match Command::new("setxkbmap").arg("-query").output() {
            Ok(out) => out,
            Err(_e) => {
                debug!("setxkbmap probe failed to spawn: {_e}");
                return None;
            }
        };
        if !output.status.success() {
            debug!(
                "setxkbmap -query exited non-zero: status={:?}",
                output.status.code()
            );
            return None;
        }

        let stdout = match String::from_utf8(output.stdout) {
            Ok(s) => s,
            Err(_e) => {
                debug!("setxkbmap -query stdout was not valid utf-8: {_e}");
                return None;
            }
        };
        let parsed = parse_setxkbmap_query(&stdout);
        if let Some(_kl) = parsed.as_ref() {
            debug!(
                "x11 layout detected via setxkbmap: layout={} variant={}",
                _kl.layout, _kl.variant
            );
        } else {
            debug!("setxkbmap -query returned no parseable layout");
        }
        parsed
    }

    /// Query `localectl status` for the system X11 layout/variant.
    ///
    /// `localectl` is a systemd binary that doesn't need `DISPLAY` or
    /// `XAUTHORITY`. On a systemd host this is the source-of-truth for
    /// the system keyboard layout when the X11 environment isn't
    /// available to the daemon (e.g. running under `systemd --user`
    /// with `DISPLAY` not imported into the user manager).
    fn from_localectl() -> Option<Self> {
        let output = match Command::new("localectl").arg("status").output() {
            Ok(out) => out,
            Err(_e) => {
                debug!("localectl probe failed to spawn: {_e}");
                return None;
            }
        };
        if !output.status.success() {
            debug!(
                "localectl status exited non-zero: status={:?}",
                output.status.code()
            );
            return None;
        }

        let stdout = match String::from_utf8(output.stdout) {
            Ok(s) => s,
            Err(_e) => {
                debug!("localectl status stdout was not valid utf-8: {_e}");
                return None;
            }
        };
        let parsed = parse_localectl_status(&stdout);
        if let Some(_kl) = parsed.as_ref() {
            debug!(
                "layout detected via localectl: layout={} variant={}",
                _kl.layout, _kl.variant
            );
        } else {
            debug!("localectl status returned no parseable X11 layout");
        }
        parsed
    }

    fn from_env() -> Option<Self> {
        let layout = std::env::var("XKB_DEFAULT_LAYOUT").ok()?;
        if layout.is_empty() {
            return None;
        }
        let variant = std::env::var("XKB_DEFAULT_VARIANT").unwrap_or_default();
        Some(Self { layout, variant })
    }
}

fn parse_setxkbmap_query(output: &str) -> Option<KeyboardLayout> {
    let mut layout = None;
    let mut variant = None;

    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "layout" => layout = Some(value.trim().to_string()),
            "variant" => variant = Some(value.trim().to_string()),
            _ => {}
        }
    }

    let layout = layout.filter(|s| !s.is_empty())?;
    Some(KeyboardLayout {
        layout,
        variant: variant.unwrap_or_default(),
    })
}

/// Parse `localectl status` output for the X11 keyboard layout/variant.
///
/// `localectl` indents its key/value lines with a varying number of
/// leading spaces (3 or 4 in practice). Trim each line before splitting
/// on the first `:` to be robust against that. Only `X11 Layout` and
/// `X11 Variant` are extracted — other keys (`System Locale`,
/// `VC Keymap`, `X11 Model`, `X11 Options`) are intentionally ignored
/// to keep this patch tightly scoped to the layout-detection bug.
fn parse_localectl_status(output: &str) -> Option<KeyboardLayout> {
    let mut layout = None;
    let mut variant = None;

    for line in output.lines() {
        let trimmed = line.trim();
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        match key.trim() {
            "X11 Layout" => layout = Some(value.trim().to_string()),
            "X11 Variant" => variant = Some(value.trim().to_string()),
            _ => {}
        }
    }

    let layout = layout.filter(|s| !s.is_empty())?;
    Some(KeyboardLayout {
        layout,
        variant: variant.unwrap_or_default(),
    })
}

pub struct XkbKeymap {
    map: HashMap<char, KeyMapping>,
    level3_keycode: u16,
    /// What each tap in `map` produces on a keyboard with this layout.
    #[cfg(feature = "xtest")]
    group1: Group1Keysyms,
}

impl XkbKeymap {
    pub fn from_layout(detected: &KeyboardLayout) -> anyhow::Result<Self> {
        let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            &detected.layout,
            &detected.variant,
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| {
            anyhow::anyhow!(
                "failed to create XKB keymap for layout '{}' variant '{}'",
                detected.layout,
                detected.variant
            )
        })?;
        // Fall back to KEY_RIGHTALT when no dedicated `<LVL3>` key exists
        // — that's the common case on plain `us`, `de`, etc. where AltGr
        // *is* RightAlt. Dedicated `<LVL3>` keycodes show up on layouts
        // like `us:qwerty-fr`.
        let level3_keycode =
            find_level3_keycode(&keymap).unwrap_or_else(|| evdev::Key::KEY_RIGHTALT.code());
        let map = build_reverse_map(&keymap);
        Ok(Self {
            map,
            level3_keycode,
            #[cfg(feature = "xtest")]
            group1: Group1Keysyms::from_keymap(&keymap),
        })
    }

    pub fn lookup(&self, ch: char) -> Option<&KeyMapping> {
        self.map.get(&ch)
    }
    /// Every character the keymap can type, with how to type it.
    #[cfg(feature = "xtest")]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (char, &KeyMapping)> {
        self.map.iter().map(|(ch, m)| (*ch, m))
    }
    /// The group 1 keysyms of this layout's keys.
    ///
    /// A character [`build_reverse_map`] found in a later group (`ф` on
    /// `us,ru`) is typed with a tap that means another keysym in group 1
    /// (`a`): that keysym is what typing it gives while group 1 is active.
    #[cfg(feature = "xtest")]
    pub(crate) fn group1(&self) -> &Group1Keysyms {
        &self.group1
    }
    /// Keep only the characters whose mapping `keep` accepts; the rest are
    /// typed like characters the layout lacks (pasted).
    #[cfg(feature = "xtest")]
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&KeyMapping) -> bool) {
        self.map.retain(|_, m| keep(m));
    }
    pub fn level3_keycode(&self) -> u16 {
        self.level3_keycode
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

use xkbcommon::xkb::keysyms::{
    KEY_ISO_Level3_Shift, KEY_dead_acute, KEY_dead_cedilla, KEY_dead_circumflex,
    KEY_dead_diaeresis, KEY_dead_grave, KEY_dead_tilde,
};

const ACCENTED_VIA_DEAD_KEY: &[(char, char, u32)] = &[
    ('ã', 'a', KEY_dead_tilde),
    ('õ', 'o', KEY_dead_tilde),
    ('ñ', 'n', KEY_dead_tilde),
    ('Ã', 'A', KEY_dead_tilde),
    ('Õ', 'O', KEY_dead_tilde),
    ('Ñ', 'N', KEY_dead_tilde),
    ('á', 'a', KEY_dead_acute),
    ('é', 'e', KEY_dead_acute),
    ('í', 'i', KEY_dead_acute),
    ('ó', 'o', KEY_dead_acute),
    ('ú', 'u', KEY_dead_acute),
    ('ý', 'y', KEY_dead_acute),
    ('Á', 'A', KEY_dead_acute),
    ('É', 'E', KEY_dead_acute),
    ('Í', 'I', KEY_dead_acute),
    ('Ó', 'O', KEY_dead_acute),
    ('Ú', 'U', KEY_dead_acute),
    ('â', 'a', KEY_dead_circumflex),
    ('ê', 'e', KEY_dead_circumflex),
    ('î', 'i', KEY_dead_circumflex),
    ('ô', 'o', KEY_dead_circumflex),
    ('û', 'u', KEY_dead_circumflex),
    ('Â', 'A', KEY_dead_circumflex),
    ('Ê', 'E', KEY_dead_circumflex),
    ('Ô', 'O', KEY_dead_circumflex),
    ('à', 'a', KEY_dead_grave),
    ('è', 'e', KEY_dead_grave),
    ('ì', 'i', KEY_dead_grave),
    ('ò', 'o', KEY_dead_grave),
    ('ù', 'u', KEY_dead_grave),
    ('À', 'A', KEY_dead_grave),
    ('ä', 'a', KEY_dead_diaeresis),
    ('ë', 'e', KEY_dead_diaeresis),
    ('ï', 'i', KEY_dead_diaeresis),
    ('ö', 'o', KEY_dead_diaeresis),
    ('ü', 'u', KEY_dead_diaeresis),
    ('ç', 'c', KEY_dead_cedilla),
    ('Ç', 'C', KEY_dead_cedilla),
];

/// Scan the keymap for a key whose **base-level** (layout 0, level 0)
/// keysym is `ISO_Level3_Shift`. Returns the evdev keycode of the first
/// such key that is **not** `KEY_RIGHTALT` — that's the dedicated
/// `<LVL3>` key on layouts like `us:qwerty-fr`.
///
/// Returns `None` when there is no dedicated `<LVL3>` key (the common
/// case: `ISO_Level3_Shift` is bound to `KEY_RIGHTALT` itself, or not at
/// all). Callers fall back to `KEY_RIGHTALT` in that case.
///
/// Restricted to layout 0 / level 0 because `ISO_Level3_Shift` is a
/// modifier and lives at the base level — scanning every level would
/// occasionally pick up keys that *produce* a Level3 modifier as a
/// shifted symbol, which is not what we want.
fn find_level3_keycode(keymap: &xkbcommon::xkb::Keymap) -> Option<u16> {
    let right_alt = evdev::Key::KEY_RIGHTALT.code();

    for raw_keycode in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        let keycode = xkbcommon::xkb::Keycode::new(raw_keycode);
        let evdev_keycode: u16 = raw_keycode.saturating_sub(8).try_into().unwrap_or(u16::MAX);

        // Restrict to layout 0 / level 0 — `ISO_Level3_Shift` is a
        // modifier at the base level.
        let syms = keymap.key_get_syms_by_level(keycode, 0, 0);
        if !syms.iter().any(|sym| sym.raw() == KEY_ISO_Level3_Shift) {
            continue;
        }

        if evdev_keycode != right_alt {
            return Some(evdev_keycode);
        }
    }

    None
}

/// The group 1 keysym at each of the four levels [`build_reverse_map`]
/// uses (Shift, AltGr), of each key by evdev keycode: what a [`KeyTap`]
/// produces while group 1 is active. A level a key lacks is `NoSymbol` (0).
#[cfg(feature = "xtest")]
pub(crate) struct Group1Keysyms(pub(crate) HashMap<u16, [u32; 4]>);

#[cfg(feature = "xtest")]
impl Group1Keysyms {
    /// Read the first keysym of each level from a compiled keymap.
    pub(crate) fn from_keymap(keymap: &xkbcommon::xkb::Keymap) -> Self {
        let levels = (keymap.min_keycode().raw()..=keymap.max_keycode().raw())
            .filter_map(|raw_keycode| {
                let keycode = xkbcommon::xkb::Keycode::new(raw_keycode);
                let evdev_keycode = u16::try_from(raw_keycode.checked_sub(8)?).ok()?;
                let levels = [0, 1, 2, 3].map(|level| {
                    keymap
                        .key_get_syms_by_level(keycode, 0, level)
                        .first()
                        .map_or(0, |sym| sym.raw())
                });
                Some((evdev_keycode, levels))
            })
            .collect();
        Self(levels)
    }

    /// The keysym `tap` produces, `NoSymbol` (0) if none.
    pub(crate) fn keysym(&self, tap: &KeyTap) -> u32 {
        // Level 1 (Shift), 2 (AltGr) and 3 (both), as in `build_reverse_map`.
        let level = usize::from(tap.shift) | usize::from(tap.altgr) << 1;
        self.0.get(&tap.keycode).map_or(0, |levels| levels[level])
    }
}

#[allow(non_upper_case_globals)]
pub(crate) fn build_reverse_map(keymap: &xkbcommon::xkb::Keymap) -> HashMap<char, KeyMapping> {
    let mut map: HashMap<char, KeyMapping> = HashMap::new();
    let mut dead_keys: HashMap<u32, KeyTap> = HashMap::new();
    let min = keymap.min_keycode().raw();
    let max = keymap.max_keycode().raw();
    let num_layouts = keymap.num_layouts();

    for raw_keycode in min..=max {
        let keycode = xkbcommon::xkb::Keycode::new(raw_keycode);
        for layout in 0..num_layouts {
            let num_levels = keymap.num_levels_for_key(keycode, layout);
            for level in 0..num_levels {
                if level > 3 {
                    continue;
                }
                let syms = keymap.key_get_syms_by_level(keycode, layout, level);
                let evdev_keycode: u16 =
                    raw_keycode.saturating_sub(8).try_into().unwrap_or(u16::MAX);
                let shift = level == 1 || level == 3;
                let altgr = level == 2 || level == 3;

                for &sym in syms {
                    let raw = sym.raw();
                    if matches!(
                        raw,
                        KEY_dead_grave
                            | KEY_dead_acute
                            | KEY_dead_circumflex
                            | KEY_dead_tilde
                            | KEY_dead_diaeresis
                            | KEY_dead_cedilla
                    ) {
                        dead_keys.entry(raw).or_insert(KeyTap {
                            keycode: evdev_keycode,
                            shift,
                            altgr,
                        });
                        continue;
                    }
                    let unicode = xkbcommon::xkb::keysym_to_utf32(sym);
                    if unicode == 0 {
                        continue;
                    }
                    if let Some(ch) = char::from_u32(unicode) {
                        map.entry(ch).or_insert(KeyMapping {
                            main: KeyTap {
                                keycode: evdev_keycode,
                                shift,
                                altgr,
                            },
                            follow: None,
                        });
                    }
                }
            }
        }
    }

    // Pass 1: dead_X + Space for literal punctuation
    for (ch, dead_sym) in [
        ('\'', KEY_dead_acute),
        ('"', KEY_dead_diaeresis),
        ('~', KEY_dead_tilde),
        ('`', KEY_dead_grave),
        ('^', KEY_dead_circumflex),
    ] {
        if map.contains_key(&ch) {
            continue;
        }
        if let Some(dk) = dead_keys.get(&dead_sym) {
            map.insert(
                ch,
                KeyMapping {
                    main: *dk,
                    follow: Some(KeyTap {
                        keycode: evdev::Key::KEY_SPACE.code(),
                        shift: false,
                        altgr: false,
                    }),
                },
            );
        }
    }

    // Pass 2: dead_X + base_letter for accented letters
    for &(ch, base, dead_sym) in ACCENTED_VIA_DEAD_KEY {
        if map.contains_key(&ch) {
            continue;
        }
        let Some(dk) = dead_keys.get(&dead_sym) else {
            continue;
        };
        let Some(base_map) = map.get(&base).copied() else {
            continue;
        };
        if base_map.follow.is_some() {
            continue;
        }
        map.insert(
            ch,
            KeyMapping {
                main: *dk,
                follow: Some(base_map.main),
            },
        );
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us_layout() -> KeyboardLayout {
        KeyboardLayout {
            layout: "us".to_string(),
            variant: String::new(),
        }
    }

    fn layout(name: &str, variant: &str) -> KeyboardLayout {
        KeyboardLayout {
            layout: name.to_string(),
            variant: variant.to_string(),
        }
    }

    fn assert_key(
        km: &XkbKeymap,
        ch: char,
        expected_keycode: u16,
        expected_shift: bool,
        label: &str,
    ) {
        assert_key_full(km, ch, expected_keycode, expected_shift, false, label);
    }

    fn assert_key_full(
        km: &XkbKeymap,
        ch: char,
        expected_keycode: u16,
        expected_shift: bool,
        expected_altgr: bool,
        label: &str,
    ) {
        let mapping = km
            .lookup(ch)
            .unwrap_or_else(|| panic!("'{ch}' should be in {label} keymap"));
        assert_eq!(
            mapping.main.keycode, expected_keycode,
            "'{ch}' should be at evdev {expected_keycode} on {label}, got {}",
            mapping.main.keycode
        );
        assert_eq!(
            mapping.main.shift, expected_shift,
            "'{ch}' shift should be {expected_shift} on {label}"
        );
        assert_eq!(
            mapping.main.altgr, expected_altgr,
            "'{ch}' altgr should be {expected_altgr} on {label}"
        );
    }

    #[test]
    fn build_us_keymap() {
        let km = XkbKeymap::from_layout(&us_layout());
        if let Ok(km) = km {
            assert!(!km.is_empty(), "keymap should not be empty");
            assert!(km.lookup('a').is_some(), "'a' should be in the keymap");
        }
    }

    #[test]
    fn parse_x11_setxkbmap_query() {
        let layout = parse_setxkbmap_query(
            "rules:      evdev\nmodel:      pc104\nlayout:     us\nvariant:    qwerty-fr\n",
        )
        .unwrap();

        assert_eq!(layout.layout, "us");
        assert_eq!(layout.variant, "qwerty-fr");
    }

    #[test]
    fn us_qwerty_fr_typeable_via_uinput() {
        // Skip if the host's xkeyboard-config lacks this third-party layout.
        // Probed directly so a from_layout regression still fails here.
        let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        let installed = xkbcommon::xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "qwerty-fr",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        );
        if installed.is_none() {
            eprintln!("skipping: us(qwerty-fr) is not installed on this host");
            return;
        }
        let km = XkbKeymap::from_layout(&layout("us", "qwerty-fr")).unwrap();

        // The real contract: on us:qwerty-fr, AltGr lives behind a
        // dedicated `<LVL3>` keycode that is *not* KEY_RIGHTALT. The
        // exact evdev keycode (currently 84 / KEY_KP4) is an XKB-
        // implementation detail and could shift between xkbcommon
        // versions, so assert the property rather than the number.
        assert_ne!(
            km.level3_keycode(),
            evdev::Key::KEY_RIGHTALT.code(),
            "us:qwerty-fr must use the dedicated XKB <LVL3> keycode for \
             AltGr, not KEY_RIGHTALT"
        );
        assert_key_full(&km, 'é', 17, false, true, "US qwerty-fr");

        for ch in ['ç', 'à', 'é', 'è', 'ù'] {
            assert!(
                km.lookup(ch).is_some(),
                "'{ch}' must be reachable via uinput on us:qwerty-fr"
            );
        }
    }

    #[test]
    fn shift_mapping_for_uppercase() {
        let km = XkbKeymap::from_layout(&us_layout());
        if let Ok(km) = km {
            if let Some(mapping) = km.lookup('A') {
                assert!(
                    mapping.main.shift,
                    "uppercase 'A' should require shift on standard layouts"
                );
            }
        }
    }

    // --- QWERTZ family (y/z swapped) ---

    #[test]
    fn german_layout() {
        let km = XkbKeymap::from_layout(&layout("de", "")).unwrap();
        assert_key(&km, 'z', 21, false, "German");
        assert_key(&km, 'y', 44, false, "German");
    }

    #[test]
    fn swiss_layout() {
        let km = XkbKeymap::from_layout(&layout("ch", "")).unwrap();
        assert_key(&km, 'z', 21, false, "Swiss");
        assert_key(&km, 'y', 44, false, "Swiss");
    }

    #[test]
    fn czech_layout() {
        let km = XkbKeymap::from_layout(&layout("cz", "")).unwrap();
        assert_key(&km, 'z', 21, false, "Czech");
        assert_key(&km, 'y', 44, false, "Czech");
        assert_key(&km, 'ů', 39, false, "Czech");
    }

    #[test]
    fn slovak_layout() {
        let km = XkbKeymap::from_layout(&layout("sk", "")).unwrap();
        assert_key(&km, 'z', 21, false, "Slovak");
        assert_key(&km, 'y', 44, false, "Slovak");
        assert_key(&km, 'ô', 39, false, "Slovak");
    }

    #[test]
    fn hungarian_layout() {
        let km = XkbKeymap::from_layout(&layout("hu", "")).unwrap();
        assert_key(&km, 'z', 21, false, "Hungarian");
        assert_key(&km, 'y', 44, false, "Hungarian");
        assert_key(&km, 'ö', 11, false, "Hungarian");
        assert_key(&km, 'ü', 12, false, "Hungarian");
    }

    // --- AZERTY family (a/q and z/w swapped) ---

    #[test]
    fn french_layout() {
        let km = XkbKeymap::from_layout(&layout("fr", "")).unwrap();
        assert_key(&km, 'a', 16, false, "French");
        assert_key(&km, 'q', 30, false, "French");
        assert_key(&km, 'z', 17, false, "French");
        assert_key(&km, 'w', 44, false, "French");
    }

    #[test]
    fn belgian_layout() {
        let km = XkbKeymap::from_layout(&layout("be", "")).unwrap();
        assert_key(&km, 'a', 16, false, "Belgian");
        assert_key(&km, 'q', 30, false, "Belgian");
        assert_key(&km, 'z', 17, false, "Belgian");
        assert_key(&km, 'w', 44, false, "Belgian");
        assert_key(&km, 'm', 39, false, "Belgian");
    }

    // --- QWERTY-based with special characters ---

    #[test]
    fn spanish_layout() {
        let km = XkbKeymap::from_layout(&layout("es", "")).unwrap();
        assert_key(&km, 'ñ', 39, false, "Spanish");
    }

    #[test]
    fn portuguese_layout() {
        let km = XkbKeymap::from_layout(&layout("pt", "")).unwrap();
        assert_key(&km, 'a', 30, false, "Portuguese");
        assert_key(&km, 'z', 44, false, "Portuguese");
        assert_key(&km, 'q', 16, false, "Portuguese");
    }

    #[test]
    fn italian_layout() {
        let km = XkbKeymap::from_layout(&layout("it", "")).unwrap();
        assert_key(&km, 'a', 30, false, "Italian");
        assert_key(&km, 'z', 44, false, "Italian");
        assert_key(&km, 'q', 16, false, "Italian");
        assert_key(&km, 'w', 17, false, "Italian");
    }

    #[test]
    fn uk_layout() {
        let km = XkbKeymap::from_layout(&layout("gb", "")).unwrap();
        assert_key(&km, 'a', 30, false, "UK");
        assert_key(&km, 'z', 44, false, "UK");
        // UK has '#' without shift (evdev 43), unlike US where it's Shift+3.
        assert_key(&km, '#', 43, false, "UK");
        assert_key(&km, '£', 4, true, "UK");
    }

    // --- Nordic layouts ---

    #[test]
    fn swedish_layout() {
        let km = XkbKeymap::from_layout(&layout("se", "")).unwrap();
        assert_key(&km, 'ö', 39, false, "Swedish");
        assert_key(&km, 'ä', 40, false, "Swedish");
    }

    #[test]
    fn norwegian_layout() {
        let km = XkbKeymap::from_layout(&layout("no", "")).unwrap();
        assert_key(&km, 'ø', 39, false, "Norwegian");
        assert_key(&km, 'æ', 40, false, "Norwegian");
    }

    #[test]
    fn danish_layout() {
        let km = XkbKeymap::from_layout(&layout("dk", "")).unwrap();
        assert_key(&km, 'ø', 40, false, "Danish");
        assert_key(&km, 'æ', 39, false, "Danish");
    }

    #[test]
    fn finnish_layout() {
        let km = XkbKeymap::from_layout(&layout("fi", "")).unwrap();
        assert_key(&km, 'ö', 39, false, "Finnish");
        assert_key(&km, 'ä', 40, false, "Finnish");
    }

    // --- Eastern European ---

    #[test]
    fn polish_layout() {
        let km = XkbKeymap::from_layout(&layout("pl", "")).unwrap();
        assert_key(&km, 'a', 30, false, "Polish");
        assert_key(&km, 'z', 44, false, "Polish");
        // Polish accented characters live at level 2 (AltGr) and now go
        // through uinput directly, no clipboard fallback needed.
        assert_key_full(&km, 'ą', 30, false, true, "Polish");
        assert_key_full(&km, 'ę', 18, false, true, "Polish");
    }

    #[test]
    fn us_intl_typeable_via_uinput() {
        let km = XkbKeymap::from_layout(&KeyboardLayout {
            layout: "us".to_string(),
            variant: "intl".to_string(),
        })
        .unwrap();
        // Every character that previously had to fall back to clipboard
        // paste (and was therefore broken in terminals like Alacritty)
        // must now have a direct uinput route — either as a level 2/3
        // AltGr key, or via the dead-key + Space fallback table.
        for ch in [
            '\'', '"', '~', '`', '^', 'ç', 'á', 'é', 'í', 'ó', 'ú', 'ã', 'ñ',
        ] {
            assert!(
                km.lookup(ch).is_some(),
                "'{ch}' must be reachable via uinput on us:intl, got no mapping"
            );
        }
    }

    // --- Synthesis routing (locks in direct vs dead-key+follow paths) ---

    /// Pass 1 (literal accent via `dead_X + Space`): for each of the
    /// chars Pass 1 covers, on us:intl the char must be reachable, and
    /// — if it ended up routed through synthesis rather than a direct
    /// level mapping — the follow tap must be unmodified Space.
    /// Whether any specific char is direct vs synthesized is layout-
    /// dependent (us:intl puts some literal accents at level 2 directly),
    /// but the invariant holds: synthesized routes always end with Space.
    #[test]
    fn us_intl_pass1_chars_synthesized_routes_end_with_space() {
        let km = XkbKeymap::from_layout(&KeyboardLayout {
            layout: "us".to_string(),
            variant: "intl".to_string(),
        })
        .unwrap();
        for ch in ['\'', '"', '~', '`', '^'] {
            let mapping = km
                .lookup(ch)
                .unwrap_or_else(|| panic!("'{ch}' must be reachable on us:intl"));
            if let Some(follow) = mapping.follow {
                assert_eq!(
                    follow.keycode,
                    evdev::Key::KEY_SPACE.code(),
                    "'{ch}' was synthesized; follow tap must be SPACE \
                     (dead_X + space sequence)"
                );
                assert!(
                    !follow.shift && !follow.altgr,
                    "'{ch}' synthesis follow tap must be unmodified SPACE"
                );
            }
        }
    }

    /// Pass 2 (accented letter via `dead_X + base_letter`): `ã` is not
    /// reachable at any level on us:intl, so it must be synthesized as
    /// `dead_tilde + a`. This is the deterministic test that proves
    /// Pass 2 actually fires; the Polish/Spanish tests below cover the
    /// no-overwrite invariant for layouts that do expose the char
    /// directly.
    #[test]
    fn us_intl_tilde_letter_uses_dead_key_synthesis() {
        let km = XkbKeymap::from_layout(&KeyboardLayout {
            layout: "us".to_string(),
            variant: "intl".to_string(),
        })
        .unwrap();
        let a_main = km.lookup('a').expect("'a' must be in keymap").main;
        let mapping = km.lookup('ã').expect("ã must be reachable on us:intl");
        let follow = mapping.follow.expect(
            "ã on us:intl must be synthesized via dead_tilde + a — \
             the literal char is not at any level on this layout",
        );
        assert_eq!(
            follow.keycode, a_main.keycode,
            "ã follow tap must target the same evdev keycode as 'a' \
             (dead_tilde + a sequence)"
        );
    }

    /// Polish exposes `ą` at level 2 directly. The synthesis pass MUST
    /// NOT overwrite that direct entry — `ą` must keep `follow=None`
    /// and use AltGr, not synthesize via dead_ogonek + a.
    #[test]
    fn polish_accented_letter_is_direct_not_synthesized() {
        let km = XkbKeymap::from_layout(&layout("pl", "")).unwrap();
        let mapping = km.lookup('ą').expect("ą must be reachable on Polish");
        assert!(
            mapping.follow.is_none(),
            "ą on Polish must be a direct AltGr tap, not synthesized — \
             synthesis pass must not overwrite a direct mapping"
        );
        assert!(mapping.main.altgr, "ą on Polish must hold AltGr");
    }

    /// Spanish exposes `ñ` at level 0 directly (it's on the dedicated
    /// `ñ` key). Even though `ñ` is in `ACCENTED_VIA_DEAD_KEY`, the
    /// synthesis pass must skip it because the direct entry already
    /// exists — locks in the cross-layout no-overwrite invariant.
    #[test]
    fn spanish_enye_is_direct_not_synthesized() {
        let km = XkbKeymap::from_layout(&layout("es", "")).unwrap();
        let mapping = km.lookup('ñ').expect("ñ must be reachable on Spanish");
        assert!(
            mapping.follow.is_none(),
            "ñ on Spanish must be a direct level-0 tap, not synthesized — \
             synthesis pass must not overwrite even when the char is in \
             the synthesis table"
        );
        assert!(
            !mapping.main.altgr && !mapping.main.shift,
            "ñ on Spanish is on a dedicated key — no modifiers required"
        );
    }

    // --- Alternative Latin layouts ---

    #[test]
    fn dvorak_layout() {
        let km = XkbKeymap::from_layout(&layout("us", "dvorak")).unwrap();
        assert_key(&km, 'o', 31, false, "Dvorak");
        assert_key(&km, 'e', 32, false, "Dvorak");
        assert_key(&km, 's', 39, false, "Dvorak");
    }

    #[test]
    fn colemak_layout() {
        let km = XkbKeymap::from_layout(&layout("us", "colemak")).unwrap();
        assert_key(&km, 'f', 18, false, "Colemak");
        assert_key(&km, 'n', 36, false, "Colemak");
        assert_key(&km, 's', 32, false, "Colemak");
    }

    // --- Non-Latin layouts ---

    #[test]
    fn russian_layout() {
        let km = XkbKeymap::from_layout(&layout("ru", "")).unwrap();
        assert_key(&km, 'ф', 30, false, "Russian");
        assert_key(&km, 'я', 44, false, "Russian");
        assert_key(&km, 'й', 16, false, "Russian");
        assert_key(&km, 'ц', 17, false, "Russian");
    }

    #[test]
    fn ukrainian_layout() {
        let km = XkbKeymap::from_layout(&layout("ua", "")).unwrap();
        assert_key(&km, 'ф', 30, false, "Ukrainian");
        assert_key(&km, 'я', 44, false, "Ukrainian");
        assert_key(&km, 'й', 16, false, "Ukrainian");
        assert_key(&km, 'і', 31, false, "Ukrainian");
    }

    #[test]
    fn greek_layout() {
        let km = XkbKeymap::from_layout(&layout("gr", "")).unwrap();
        assert_key(&km, 'α', 30, false, "Greek");
        assert_key(&km, 'ζ', 44, false, "Greek");
        assert_key(&km, 'ω', 47, false, "Greek");
    }

    #[test]
    fn japanese_layout() {
        // Japanese (jp) is QWERTY-based for Latin characters.
        let km = XkbKeymap::from_layout(&layout("jp", "")).unwrap();
        assert_key(&km, 'a', 30, false, "Japanese");
        assert_key(&km, 'z', 44, false, "Japanese");
    }

    // --- Sway detection regression ---

    /// `KeyboardLayout::from_sway` deliberately returns `None` because
    /// Sway's `xkb_layout_names` exposes display strings (e.g. `"German"`,
    /// `"English (US)"`) rather than XKB layout codes (`"de"`, `"us"`).
    /// Returning `Some(display_name)` would make the caller short-circuit
    /// the env-var fallback and silently compile against the *default*
    /// layout, which is the bug this regression test guards against.
    #[test]
    fn from_sway_always_returns_none() {
        // `from_sway` shells out to `swaymsg`; on most CI machines that
        // command doesn't exist, but if it does we must still see `None`
        // (the function intentionally never returns `Some` regardless of
        // what swaymsg replies with).
        let result = KeyboardLayout::from_sway();
        assert!(
            result.is_none(),
            "from_sway must return None (display names ≠ XKB codes); got {result:?}"
        );
    }

    // --- localectl parser tests ---

    #[test]
    fn parse_localectl_us_no_variant() {
        let output = "\
System Locale: LANG=en_US.UTF-8
    VC Keymap: us
   X11 Layout: us
    X11 Model: pc105+inet
  X11 Options: terminate:ctrl_alt_bksp
";
        let kl = parse_localectl_status(output).expect("should parse US layout");
        assert_eq!(kl.layout, "us");
        assert_eq!(kl.variant, "");
    }

    #[test]
    fn parse_localectl_fr_bepo_full() {
        let output = "\
System Locale: LANG=en_US.UTF-8
    VC Keymap: fr-bepo
   X11 Layout: fr
    X11 Model: pc104
  X11 Variant: bepo
  X11 Options: caps:swapescape,compose:rwin
";
        let kl = parse_localectl_status(output).expect("should parse fr(bepo)");
        assert_eq!(kl.layout, "fr");
        assert_eq!(kl.variant, "bepo");
    }

    #[test]
    fn parse_localectl_empty_layout_returns_none() {
        let output = "\
System Locale: LANG=en_US.UTF-8
    VC Keymap: us
   X11 Layout:
    X11 Model: pc105
";
        assert!(parse_localectl_status(output).is_none());
    }

    #[test]
    fn parse_localectl_missing_x11_section_returns_none() {
        let output = "\
System Locale: LANG=en_US.UTF-8
    VC Keymap: us
";
        assert!(parse_localectl_status(output).is_none());
    }

    #[test]
    fn parse_localectl_tolerates_whitespace_variants() {
        // Mix tabs, varying leading-space widths, and trailing whitespace.
        let output = "System Locale: LANG=en_US.UTF-8\n\
\t\tVC Keymap: fr-bepo\n\
 X11 Layout:   fr  \n\
      X11 Model: pc104\n\
\tX11 Variant:\tbepo\n";
        let kl = parse_localectl_status(output).expect("should tolerate whitespace mix");
        assert_eq!(kl.layout, "fr");
        assert_eq!(kl.variant, "bepo");
    }

    #[test]
    fn parse_localectl_ignores_unknown_keys() {
        let output = "\
System Locale: LANG=en_US.UTF-8
    VC Keymap: us
          Foo: bar
   X11 Layout: us
    X11 Model: pc105
       Random: line with: multiple colons
  X11 Variant: dvorak
";
        let kl = parse_localectl_status(output).expect("should ignore unknown keys");
        assert_eq!(kl.layout, "us");
        assert_eq!(kl.variant, "dvorak");
    }

    // --- localectl chain-gap regression ---

    /// Issue #39: detection must have at least one source that works
    /// without any X session env vars on a systemd host. `from_localectl`
    /// fills that gap. If `localectl` is installed (the case on every
    /// systemd distro), it must return `Some(layout)` with a non-empty
    /// layout code — this is what prevents the US/QWERTY fallback that
    /// produced garbled output for fr(bepo) users running under
    /// `systemd --user`.
    #[test]
    fn from_localectl_returns_layout_when_available() {
        // Skip if the binary is unavailable (e.g. non-systemd CI image).
        if Command::new("localectl").arg("--version").output().is_err() {
            eprintln!("skipping: localectl not on PATH");
            return;
        }
        let result = KeyboardLayout::from_localectl();
        let kl = result.expect(
            "from_localectl must return Some(_) on a systemd host — \
             this is the detection-chain gap that allowed issue #39",
        );
        assert!(
            !kl.layout.is_empty(),
            "from_localectl returned Some with empty layout: {kl:?}"
        );
    }
}
