//! XKB reverse lookup table: char → (Keycode, Modifiers).
//!
//! Uses `xkbcommon` to read the active keyboard layout and build a reverse
//! mapping so we know which physical key (+ shift) produces each character.

use std::collections::HashMap;

use tracing::debug;

use super::KeyMapping;

/// Reverse lookup table from character to the key event needed to produce it.
pub struct XkbKeymap {
    map: HashMap<char, KeyMapping>,
}

impl XkbKeymap {
    /// Build the reverse keymap from the system's default XKB layout.
    pub fn from_default_layout() -> anyhow::Result<Self> {
        let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);

        // Create keymap from default RMLVO names (reads the system layout).
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &context,
            "",   // rules — empty string means default
            "",   // model
            "",   // layout
            "",   // variant
            None, // options
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow::anyhow!("failed to create XKB keymap from default layout"))?;

        let map = build_reverse_map(&keymap);
        debug!("built XKB reverse keymap with {} entries", map.len());

        Ok(Self { map })
    }

    /// Look up the key mapping for a character.
    pub fn lookup(&self, ch: char) -> Option<&KeyMapping> {
        self.map.get(&ch)
    }

    /// Number of entries in the keymap.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the keymap is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Iterate all keycodes and shift levels to build a `char → KeyMapping` table.
fn build_reverse_map(keymap: &xkbcommon::xkb::Keymap) -> HashMap<char, KeyMapping> {
    let mut map = HashMap::new();

    // xkb keycodes: iterate from min to max.
    let min = keymap.min_keycode().raw();
    let max = keymap.max_keycode().raw();

    let num_layouts = keymap.num_layouts();

    for raw_keycode in min..=max {
        let keycode = xkbcommon::xkb::Keycode::new(raw_keycode);

        for layout in 0..num_layouts {
            let num_levels = keymap.num_levels_for_key(keycode, layout);

            for level in 0..num_levels {
                let syms = keymap.key_get_syms_by_level(keycode, layout, level);

                for &sym in syms {
                    let unicode = xkbcommon::xkb::keysym_to_utf32(sym);
                    if unicode == 0 {
                        continue;
                    }

                    if let Some(ch) = char::from_u32(unicode) {
                        // The evdev keycode is the XKB keycode minus 8
                        // (XKB adds 8 to Linux input keycodes).
                        let evdev_keycode = raw_keycode.saturating_sub(8);

                        // Level 0 = no modifiers, Level 1 = Shift
                        let shift = level >= 1;

                        let mapping = KeyMapping {
                            keycode: evdev_keycode as u16,
                            shift,
                        };

                        // Prefer un-shifted mappings (level 0) over shifted ones.
                        // Only insert if not already present (first-come wins,
                        // and level 0 is iterated first).
                        map.entry(ch).or_insert(mapping);
                    }
                }
            }
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_default_keymap() {
        // This test requires libxkbcommon to be installed.
        // It may fail in CI without the system library.
        let km = XkbKeymap::from_default_layout();
        if let Ok(km) = km {
            // Should have at least the basic ASCII letters.
            assert!(!km.is_empty(), "keymap should not be empty");
            // 'a' should be mappable on any standard layout.
            assert!(km.lookup('a').is_some(), "'a' should be in the keymap");
        }
        // If xkbcommon is not available, skip gracefully.
    }

    #[test]
    fn shift_mapping_for_uppercase() {
        let km = XkbKeymap::from_default_layout();
        if let Ok(km) = km {
            if let Some(mapping) = km.lookup('A') {
                assert!(
                    mapping.shift,
                    "uppercase 'A' should require shift on standard layouts"
                );
            }
        }
    }
}
