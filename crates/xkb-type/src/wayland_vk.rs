//! Layout-independent text injection via the Wayland
//! `zwp_virtual_keyboard_v1` protocol (the "wtype" model).
//!
//! # Why this exists
//!
//! The uinput backend ([`crate::Keyboard`]) emits raw evdev keycodes. The
//! compositor then reinterprets those keycodes through the user's **active**
//! keyboard layout. For bilingual text (e.g. mixed Latin + Arabic) this
//! garbles output: a keycode that produces `a` under a US layout produces a
//! completely different glyph under an Arabic layout.
//!
//! The virtual-keyboard protocol sidesteps this entirely. We upload our
//! **own** XKB keymap in which every character we want to type sits on its
//! own keycode at level 0 (no modifiers), with the keysym *being* that exact
//! glyph. Pressing that keycode against our uploaded keymap therefore yields
//! the intended character regardless of the user's configured layout — and
//! because these are real key events, they still work in terminals (unlike
//! clipboard paste).
//!
//! # The keymap model
//!
//! Each safe keycode is a `FOUR_LEVEL` key carrying up to three symbols:
//!
//! * **Level 0 (no modifier)** and **level 1 (Shift)** hold the *stock `us`*
//!   base and shifted symbols, so **all printable ASCII** — lowercase,
//!   UPPERCASE, digits, common punctuation, and space — is **permanent** in
//!   every uploaded keymap ([`PERMANENT_ASCII_KEYS`]). ASCII therefore never
//!   depends on a mid-utterance keymap re-upload and can never be dropped by the
//!   keymap-apply race or squeezed out by the dynamic-capacity limit. Because
//!   these levels are byte-for-byte stock `us`, they are exactly as bind-safe
//!   as a real US keyboard.
//! * **Level 2 (AltGr / `ISO_Level3_Shift`)** holds one **dynamic** non-ASCII
//!   glyph (Arabic, Cyrillic, …). Dynamics are assigned, in first-seen order,
//!   to the AltGr slot of the *safe alphanumeric block*
//!   ([`SAFE_ALNUM_XKB_KEYCODES`]) — never a high keycode that would resolve to
//!   `Print` / an F-key / a media key in the user's configured layout (the
//!   compositor evaluates keybinds against *that* layout, not our uploaded
//!   keymap). See [`SAFE_ALNUM_XKB_KEYCODES`] for the rationale and citations.
//! * A small fixed **base** of real `evdev_keycode + 8` positions keeps
//!   BackSpace, Ctrl, Shift, Alt, AltGr, Return, and Tab working with the
//!   standard keysyms a compositor expects; combos (`Ctrl+V/C/A/K`) reach their
//!   letters through the permanent ASCII keys.
//!
//! We declare Shift / AltGr active via [`ZwpVirtualKeyboardV1::modifiers`] (the
//! *wtype* model — a depressed-modifier mask) rather than pressing a physical
//! modifier key, because the mask is applied atomically for level selection
//! whereas a modifier-key press can race the letter that follows it.
//!
//! Only a *genuinely new* non-ASCII glyph triggers a keymap re-upload, and each
//! re-upload is synced + settled ([`KEYMAP_SETTLE`]) so no key is ever emitted
//! before its keymap is live. If the running set of distinct non-ASCII glyphs
//! would exceed [`dynamic_capacity`], the oldest unused glyphs are evicted to
//! make room (never a glyph the current delta needs); only a single
//! [`type_text`](WaylandVkKeyboard::type_text) call whose *own* distinct
//! non-ASCII count exceeds capacity falls back to per-batch uploads — still
//! synced, still never a silent drop.
//!
//! # Keycode units: XKB in the keymap, evdev on the wire
//!
//! Every keycode in the uploaded keymap and in [`WaylandVkKeyboard`]'s
//! `char → CharKey` map is an **XKB** keycode. But `zwp_virtual_keyboard_v1.key`
//! inherits the `wl_keyboard.key` convention: its `keycode` argument is an
//! **evdev** keycode, and the compositor adds `8` (XKB keycode = evdev + 8)
//! before the keymap lookup. So the value transmitted on the wire is
//! `xkb_keycode - 8` ([`xkb_to_evdev_keycode`], applied in
//! [`WaylandVkKeyboard::emit_key`]). Sending the raw XKB keycode instead — the
//! original bug — made the compositor look up `xkb_keycode + 8`, i.e. 8 keys
//! away, producing random glyphs and tripping unrelated keysym/keycode binds
//! (notably the screenshot key). See [`EVDEV_XKB_OFFSET`] for citations.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use xkbcommon::xkb;

use crate::KeyInjector;

// Logging via the `log` crate (only active with `logging` feature), matching
// the conditional shim used in `keyboard.rs`/`keymap.rs`.
#[cfg(feature = "logging")]
use log::{debug, warn};
#[cfg(not(feature = "logging"))]
macro_rules! debug {
    ($($arg:tt)*) => {};
}
#[cfg(not(feature = "logging"))]
macro_rules! warn {
    ($($arg:tt)*) => {};
}

/// XKB keymap format value for `XKB_KEYMAP_FORMAT_TEXT_V1` (per the protocol
/// `keymap_format` enum). Passed to `zwp_virtual_keyboard_v1.keymap`.
const KEYMAP_FORMAT_TEXT_V1: u32 = 1;

/// Key-state values for `zwp_virtual_keyboard_v1.key` (mirrors `wl_keyboard`).
const KEY_STATE_RELEASED: u32 = 0;
const KEY_STATE_PRESSED: u32 = 1;

/// Bounded settle after a keymap upload, before any key is emitted.
///
/// A `roundtrip` guarantees the *server* has received and processed the keymap.
/// On wlroots/sway that is sufficient — the headless streaming stress test
/// passes 100% even at `0 ms` — because the keymap is applied synchronously and
/// the roundtrip flushes the re-broadcast to the focused client. But on some
/// compositors (notably Hyprland, where the original intermittent-drop bug was
/// observed; cf. the analogous labwc virtual-keyboard re-broadcast race) the
/// new keymap can lag behind key interpretation. This small, *bounded* settle
/// closes that window with margin.
///
/// It is paid **only on a genuine re-upload** (when a new non-ASCII glyph first
/// appears, or under over-capacity eviction) — never on steady-state ASCII or
/// reused-glyph typing, which do not re-upload at all — so it has no effect on
/// throughput for the common case. 20 ms is conservative headroom over the
/// `0 ms` that already passes on sway.
const KEYMAP_SETTLE: Duration = Duration::from_millis(20);

/// Offset between an evdev/Linux keycode and the corresponding XKB keycode.
///
/// `zwp_virtual_keyboard_v1.key` inherits the `wl_keyboard.key` convention: the
/// `keycode` argument is an **evdev** keycode. The compositor adds `8` to it
/// before looking the key up in the uploaded XKB keymap (XKB keycode =
/// evdev + 8). This is confirmed by:
///
/// * **wlroots** `wlr_keyboard_notify_key` — `uint32_t keycode = event->keycode
///   + 8; xkb_state_update_key(...)` (`types/wlr_keyboard.c`).
/// * **wtype** (the reference implementation) — its keymap defines a glyph at
///   XKB keycode `i + 8 + 1` but passes `i + 1` to `zwp_virtual_keyboard_v1.key`,
///   i.e. it sends `xkb_keycode - 8` (`main.c`).
///
/// Our keymap therefore defines every symbol at an XKB keycode, and every value
/// handed to [`WaylandVkKeyboard::emit_key`] is an XKB keycode, but the value
/// actually transmitted on the wire is `xkb_keycode - EVDEV_XKB_OFFSET`. Sending
/// the raw XKB keycode (the original bug) made the compositor look up
/// `xkb_keycode + 8`, landing 8 keys away — producing garbage and tripping
/// keysym/keycode binds (e.g. the screenshot key).
const EVDEV_XKB_OFFSET: u32 = 8;

/// Convert an XKB keycode (as used throughout the keymap and the
/// `char → keycode` map) into the evdev keycode that must be sent to
/// `zwp_virtual_keyboard_v1.key`.
///
/// The compositor re-adds [`EVDEV_XKB_OFFSET`] before the keymap lookup, so this
/// is the exact inverse of how symbols are placed in [`build_keymap_string`]
/// (XKB keycode = evdev + 8). Pure and unit-testable so the keycode arithmetic
/// is locked against regressions (see `vk_key_arg_plus_8_equals_xkb_keycode`).
///
/// XKB keycodes are always `>= 8` (`minimum = 8` in the uploaded keymap and the
/// `evdev + 8` base-key positions), so the subtraction never underflows.
pub(crate) fn xkb_to_evdev_keycode(xkb_keycode: u32) -> u32 {
    debug_assert!(
        xkb_keycode >= EVDEV_XKB_OFFSET,
        "XKB keycode {xkb_keycode} is below the evdev offset {EVDEV_XKB_OFFSET}"
    );
    xkb_keycode.saturating_sub(EVDEV_XKB_OFFSET)
}

/// XKB keycode of the Shift key, present in every keymap and bound via
/// `modifier_map Shift`. `KEY_LEFTSHIFT` (evdev 42) + 8. We declare Shift active
/// through [`ZwpVirtualKeyboardV1::modifiers`] (the wtype model) rather than
/// pressing this key, but the key must exist so the `modifier_map` resolves.
const SHIFT_XKB_KEYCODE: u32 = 50;

/// XKB keycode of the AltGr / `ISO_Level3_Shift` key, present in every keymap
/// and bound via `modifier_map Mod5`. `KEY_RIGHTALT` (evdev 100) + 8. Mirrors
/// the uinput backend (`keymap.rs`), which also drives third-level glyphs
/// through `KEY_RIGHTALT`. See the bind-safety note on [`PERMANENT_ASCII_KEYS`].
const ALTGR_XKB_KEYCODE: u32 = 108;

/// Modifier **mask** (the depressed-modifiers bitfield passed to
/// `zwp_virtual_keyboard_v1.modifiers`) that selects **level 1** (Shift).
///
/// Our keymap binds `modifier_map Shift { <K50> }`, so Shift is the real
/// modifier at index 0 — mask bit `1 << 0`. Fixed by the keymap, not
/// layout-dependent (asserted in `modifier_masks_select_levels`).
const SHIFT_MOD_MASK: u32 = 1 << 0;

/// Modifier **mask** that selects **level 2** (AltGr / `ISO_Level3_Shift`).
///
/// Our keymap binds `modifier_map Mod5 { <K108> }`, so AltGr is `Mod5` at index
/// 7 — mask bit `1 << 7`. Asserted in `modifier_masks_select_levels`.
const ALTGR_MOD_MASK: u32 = 1 << 7;

/// XKB keycode of the Control key, present in every keymap and bound via
/// `modifier_map Control`. `KEY_LEFTCTRL` (evdev 29) + 8.
const CTRL_XKB_KEYCODE: u32 = 37;

/// Modifier **mask** that declares Control depressed, for combos like
/// Ctrl+V sent via [`WaylandVkKeyboard::send_combo`].
///
/// Our keymap binds `modifier_map Control { <K37> }`, so Control is the real
/// modifier at its standard index 2 — mask bit `1 << 2`. Asserted in
/// `modifier_masks_select_levels`.
///
/// `send_combo` declares this mask via `zwp_virtual_keyboard_v1.modifiers`
/// rather than pressing the Ctrl keycode directly, for the same reason
/// [`WaylandVkKeyboard::tap_char_key`] declares Shift/AltGr that way: without
/// `modifier_map Control` binding a keycode to the real Control modifier, and
/// without explicitly asserting the mask, the compositor has nothing telling
/// it that a pressed Ctrl keycode means "Control is down" — the receiving
/// client would see a bare key press instead of a Ctrl-chord.
const CTRL_MOD_MASK: u32 = 1 << 2;

/// The XKB level reached by a per-character key tap, encoding which modifier
/// (if any) must be held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    /// Level 0: no modifier (lowercase letters, digits, base punctuation).
    Base,
    /// Level 1: Shift held (uppercase letters, shifted punctuation).
    Shift,
    /// Level 2: AltGr (`ISO_Level3_Shift`) held (dynamic non-ASCII glyphs).
    AltGr,
}

/// Where a character lives in the uploaded keymap: a keycode plus the level
/// (and therefore the modifier) needed to produce it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CharKey {
    pub(crate) keycode: u32,
    pub(crate) level: Level,
}

/// The **permanent** printable-ASCII layout, present in *every* uploaded
/// keymap. Each entry is `(xkb_keycode, level0_keysym_name, level1_keysym_name)`
/// taken verbatim from a stock `us` (`evdev`/`pc105`) keymap, so these keys
/// resolve **identically to stock `us`** — and are therefore exactly as
/// bind-safe as a real US keyboard.
///
/// Making ASCII permanent is the core of the robustness fix: lowercase,
/// UPPERCASE, digits, common punctuation, and space never depend on a
/// mid-utterance keymap re-upload, so they can never be dropped by the
/// keymap-apply race or squeezed out by the dynamic-capacity limit. Only
/// genuinely non-ASCII glyphs (Arabic, Cyrillic, …) remain dynamic.
///
/// Each of these keycodes is also a slot for one dynamic non-ASCII glyph at
/// **level 2** (AltGr), via the `FOUR_LEVEL` key type — see
/// [`build_keymap_string`].
///
/// # Bind-safety
///
/// * Level 0 / level 1 are byte-for-byte the stock `us` symbols, so a bare or
///   Shifted keystroke matches stock `us` exactly: a compositor that evaluates
///   binds against the user's `us` layout sees an ordinary letter/digit/punct,
///   never a `Print`/F-key/media/modifier key. (This is the same property the
///   previous scheme proved for level 0; we extend it to Shift.)
/// * The Shift we emit for uppercase carries **only** Shift — never Super/Ctrl —
///   so it cannot match the modifier-bearing binds compositors actually use.
/// * Level 2 carries **only** AltGr (`ISO_Level3_Shift`). AltGr-only binds are
///   rare; the residual risk is documented in the module tests.
const PERMANENT_ASCII_KEYS: &[(u32, &str, &str)] = &[
    // number row <AE01>..<AE12>: digits + shifted symbols
    (10, "1", "exclam"),
    (11, "2", "at"),
    (12, "3", "numbersign"),
    (13, "4", "dollar"),
    (14, "5", "percent"),
    (15, "6", "asciicircum"),
    (16, "7", "ampersand"),
    (17, "8", "asterisk"),
    (18, "9", "parenleft"),
    (19, "0", "parenright"),
    (20, "minus", "underscore"),
    (21, "equal", "plus"),
    // top letter row <AD01>..<AD12>
    (24, "q", "Q"),
    (25, "w", "W"),
    (26, "e", "E"),
    (27, "r", "R"),
    (28, "t", "T"),
    (29, "y", "Y"),
    (30, "u", "U"),
    (31, "i", "I"),
    (32, "o", "O"),
    (33, "p", "P"),
    (34, "bracketleft", "braceleft"),
    (35, "bracketright", "braceright"),
    // home row <AC01>..<AC11>
    (38, "a", "A"),
    (39, "s", "S"),
    (40, "d", "D"),
    (41, "f", "F"),
    (42, "g", "G"),
    (43, "h", "H"),
    (44, "j", "J"),
    (45, "k", "K"),
    (46, "l", "L"),
    (47, "semicolon", "colon"),
    (48, "apostrophe", "quotedbl"),
    // bottom row <AB01>..<AB10>
    (52, "z", "Z"),
    (53, "x", "X"),
    (54, "c", "C"),
    (55, "v", "V"),
    (56, "b", "B"),
    (57, "n", "N"),
    (58, "m", "M"),
    (59, "comma", "less"),
    (60, "period", "greater"),
    (61, "slash", "question"),
    // The remaining printable-ASCII characters live on keycodes *outside* the
    // safe alphanumeric block (so they cannot host a dynamic glyph), but they
    // are still plain printable keys in stock `us`, hence bind-safe.
    (49, "grave", "asciitilde"), // <TLDE> ` / ~
    (51, "backslash", "bar"),    // <BKSL> \ / |
    (65, "space", "space"),      // <SPCE> space (both levels = space)
];

/// The keycodes that may host a **dynamic non-ASCII glyph at level 2**: the
/// safe alphanumeric block. These are exactly the `PERMANENT_ASCII_KEYS`
/// entries whose keycode is in [`SAFE_ALNUM_XKB_KEYCODES`] (i.e. everything
/// except the out-of-block grave/backslash/space keys).
///
/// The pool of XKB keycodes that dynamic glyphs may be assigned to.
///
/// # Why not just "any high keycode"
///
/// The original scheme used a dense high block (`DYNAMIC_BASE = 200`,
/// XKB `200..=255` → evdev `192..=247`). Our uploaded keymap *did* map those
/// keycodes to our glyphs, so the focused client received the right character.
/// But a compositor does **not** evaluate its keybinds against our virtual
/// keyboard's keymap. Hyprland resolves the bind keysym with
/// `xkb_state_key_get_one_sym(m_xkbTranslationState, evdev + 8)`, where
/// `m_xkbTranslationState` is the **KeybindManager's** state, built from the
/// *user's configured* layout via `xkb_keymap_new_from_names2` — independent of
/// any virtual keyboard (`src/managers/KeybindManager.cpp` `onKeyEvent`;
/// `m_resolveBindsBySym` defaults to false). In a stock `us` keymap those high
/// keycodes resolve to **special keysyms**: evdev 210 (XKB 218) → `Print`,
/// evdev 192/193/194 → `F22`/`F23`/`F24`, and a long run of `XF86*` media keys.
/// Keysym-configured binds match by keysym (`KeybindManager.cpp`
/// `xkb_keysym_from_name(k->key) == key.keysym`), so the `Print`/screenshot
/// bind fired on every keystroke that landed on evdev 210 — exactly the
/// reported symptom.
///
/// # The safe block
///
/// We therefore restrict dynamic glyphs to the **standard alphanumeric block**:
/// the number row and the three letter rows of a PC keyboard. In *every*
/// standard keymap these keycodes resolve, at level 0 with no modifier, to a
/// plain printable symbol (digit / letter / punctuation) — never a
/// function/media/`Print`/modifier key — so they cannot match a special-key
/// bind. And because real keybinds essentially always carry a modifier
/// (`SUPER`, `CTRL`, …) while our injected keys carry none, a bare digit/letter
/// keystroke does not trip ordinary binds either.
///
/// The XKB keycodes below are `evdev + 8` for the standard `evdev` keycodes
/// file (verified against `/usr/share/X11/xkb/keycodes/evdev` and a compiled
/// stock `us` keymap):
///
/// * `10..=21`  — `<AE01>..<AE12>` number row: `1 2 3 4 5 6 7 8 9 0 - =`
/// * `24..=35`  — `<AD01>..<AD12>` top letter row: `q w e r t y u i o p [ ]`
/// * `38..=48`  — `<AC01>..<AC11>` home row: `a s d f g h j k l ; '`
/// * `52..=61`  — `<AB01>..<AB10>` bottom row: `z x c v b n m , . /`
///
/// Keycodes already claimed by a [`base_keys`] entry (the combo letters
/// `v c a k`, which live at their real `evdev + 8` positions inside this block)
/// are filtered out by [`dynamic_keycode_pool`] so a glyph never collides with a
/// base key.
const SAFE_ALNUM_XKB_KEYCODES: &[u32] = &[
    // number row <AE01>..<AE12>
    10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, //
    // top letter row <AD01>..<AD12>
    24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, //
    // home row <AC01>..<AC11>
    38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, //
    // bottom row <AB01>..<AB10>
    52, 53, 54, 55, 56, 57, 58, 59, 60, 61,
];

/// The dynamic-glyph keycode pool: the safe alphanumeric block
/// ([`SAFE_ALNUM_XKB_KEYCODES`]), preserving order so keycode assignment is
/// deterministic.
///
/// Each of these keycodes hosts a permanent ASCII pair at level 0/1 *and* one
/// dynamic non-ASCII glyph at **level 2** (AltGr). Because dynamics now live on
/// a distinct level rather than consuming a whole keycode, the full safe block
/// is available — no keycode is reserved away for combo letters (those are the
/// permanent ASCII `a/c/v/k` keys, reached at level 0).
fn dynamic_keycode_pool() -> Vec<u32> {
    SAFE_ALNUM_XKB_KEYCODES.to_vec()
}

/// Number of distinct **non-ASCII** characters that fit in one uploaded keymap.
///
/// Equals the size of [`dynamic_keycode_pool`] (the safe alphanumeric block):
/// one AltGr-level slot per safe keycode. ASCII does **not** count against this
/// — it is permanent. Strings with more distinct *non-ASCII* glyphs than this
/// are typed in batches (see [`WaylandVkKeyboard::type_text_batched`]) — never
/// silently dropped.
fn dynamic_capacity() -> usize {
    dynamic_keycode_pool().len()
}

/// True if `ch` is a printable ASCII character that the permanent layout
/// covers (it lives in [`PERMANENT_ASCII_KEYS`] at level 0 or 1). These never
/// trigger a keymap re-upload.
pub(crate) fn is_permanent_ascii(ch: char) -> bool {
    permanent_ascii_map().contains_key(&ch)
}

/// Build the `char → CharKey` map for every printable-ASCII character, derived
/// once from [`PERMANENT_ASCII_KEYS`] by compiling its keysym names back to
/// glyphs. Level 0 keysym → [`Level::Base`]; level 1 keysym → [`Level::Shift`].
///
/// Pure and dependency-free (uses `xkb::keysym_from_name` + `keysym_to_utf32`),
/// so it is unit-testable without a Wayland connection.
pub(crate) fn permanent_ascii_map() -> HashMap<char, CharKey> {
    let mut map = HashMap::new();
    for &(keycode, l0, l1) in PERMANENT_ASCII_KEYS {
        for (name, level) in [(l0, Level::Base), (l1, Level::Shift)] {
            let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
            let cp = xkb::keysym_to_utf32(sym);
            if cp == 0 {
                continue;
            }
            if let Some(ch) = char::from_u32(cp) {
                // First-seen wins, so level 0 (Base) is preferred over level 1
                // when a glyph appears at both (e.g. space at both levels).
                map.entry(ch).or_insert(CharKey { keycode, level });
            }
        }
    }
    map
}

/// A reserved base key: a standard keysym pinned to its true
/// `evdev_keycode + 8` position so the uploaded keymap behaves like a real
/// keyboard for modifiers, backspace, and combo letters.
struct BaseKey {
    /// XKB keycode (`evdev code + 8`).
    keycode: u32,
    /// XKB keysym name (as produced by `keysym_get_name`).
    keysym_name: &'static str,
}

/// The fixed base keys present in every uploaded keymap.
///
/// Each entry pins a standard keysym to the keycode the compositor would see
/// from a real keyboard (`evdev code + 8`), so combos like Ctrl+V / Ctrl+C /
/// Ctrl+A and Backspace produce the expected effect even though the rest of
/// the keymap is synthetic.
fn base_keys() -> Vec<BaseKey> {
    use evdev::Key;
    // (evdev key, keysym name) pairs. Keysym names are the canonical XKB
    // names accepted in an `xkb_symbols` block.
    //
    // The combo letters (`v c a k`) are NOT here anymore: they are part of the
    // permanent ASCII layout ([`PERMANENT_ASCII_KEYS`]) at level 0, so
    // `send_combo` reaches them through their real `evdev + 8` keycodes just the
    // same. Shift and AltGr are the modifiers we press to reach levels 1 and 2.
    let entries: &[(Key, &'static str)] = &[
        (Key::KEY_BACKSPACE, "BackSpace"),
        (Key::KEY_LEFTCTRL, "Control_L"),
        (Key::KEY_RIGHTCTRL, "Control_R"),
        (Key::KEY_LEFTSHIFT, "Shift_L"),
        (Key::KEY_LEFTALT, "Alt_L"),
        // AltGr: drives third-level (non-ASCII) glyphs. `KEY_RIGHTALT` + 8.
        (Key::KEY_RIGHTALT, "ISO_Level3_Shift"),
        // Whitespace keys that have no XKB keysym via `utf32_to_keysym`
        // (`\n`/`\t`) so they are typed through their real evdev positions
        // instead of being silently dropped (matches the uinput backend).
        (Key::KEY_ENTER, "Return"),
        (Key::KEY_TAB, "Tab"),
    ];
    entries
        .iter()
        .map(|(key, name)| BaseKey {
            keycode: u32::from(key.code()) + 8,
            keysym_name: name,
        })
        .collect()
}

/// Build the full XKB keymap text for the permanent ASCII layout plus the
/// given dynamic non-ASCII `chars`, and return the keymap string together with
/// the `char → CharKey` map covering **both** the permanent ASCII characters
/// and the placed dynamic characters.
///
/// # Structure
///
/// * Every [`PERMANENT_ASCII_KEYS`] keycode is emitted with the `FOUR_LEVEL`
///   key type and symbol list `[ base, shifted, dynamic ]` — level 0 = the
///   stock-`us` base symbol (lowercase / digit / punct), level 1 = the
///   stock-`us` Shift symbol (UPPERCASE / shifted punct), level 2 = a dynamic
///   non-ASCII glyph (or `NoSymbol` if that keycode has none). ASCII is
///   therefore **permanent** and never re-uploaded.
/// * Dynamic non-ASCII glyphs are placed at level 2 of the safe alphanumeric
///   keycodes ([`dynamic_keycode_pool`]) in first-seen order, so existing
///   glyphs keep their slot across re-uploads. Glyphs with no XKB keysym are
///   skipped (warned once). At most [`dynamic_capacity`] are placed; any beyond
///   that are ignored here (callers batch).
/// * Base/modifier keys (BackSpace, Ctrl, Shift, Alt, AltGr, Return, Tab) are
///   pinned at their real `evdev + 8` positions. Shift, AltGr, and Control
///   each have a `modifier_map` entry making them *real* modifiers — Shift and
///   AltGr so declaring their mask reaches level 1/2, Control so `send_combo`
///   can declare it depressed for chords like Ctrl+V.
///
/// This is factored out as a pure function so it can be unit-tested without a
/// Wayland connection (see the roundtrip tests).
pub(crate) fn build_keymap_string(chars: &[char]) -> (String, HashMap<char, CharKey>) {
    let base = base_keys();

    // Start from the permanent ASCII map; dynamic glyphs are added below.
    let mut char_to_key: HashMap<char, CharKey> = permanent_ascii_map();

    // Assign each distinct, typeable, *non-ASCII* dynamic glyph to the next
    // free safe keycode's level-2 (AltGr) slot, in first-seen order. ASCII is
    // skipped here — it is already permanent. Keysym-less glyphs are skipped.
    let pool = dynamic_keycode_pool();
    let mut pool_iter = pool.iter().copied();
    // keycode -> dynamic keysym name (the level-2 symbol for that key).
    let mut dynamic_level2: HashMap<u32, String> = HashMap::new();
    let mut placed_dynamic: std::collections::HashSet<char> = std::collections::HashSet::new();

    for &ch in chars {
        if is_permanent_ascii(ch) || placed_dynamic.contains(&ch) {
            continue;
        }
        let keysym = xkb::utf32_to_keysym(ch as u32);
        if keysym == xkb::keysyms::KEY_NoSymbol.into() {
            warn!("wayland-vk: no keysym for char {ch:?}; skipping");
            continue;
        }
        let name = xkb::keysym_get_name(keysym);
        if name.is_empty() {
            warn!("wayland-vk: empty keysym name for char {ch:?}; skipping");
            continue;
        }
        // Take the next safe keycode's AltGr slot. When the pool is exhausted
        // the keymap is full; stop and let the caller batch the remainder.
        let Some(keycode) = pool_iter.next() else {
            break;
        };
        dynamic_level2.insert(keycode, name);
        char_to_key.insert(
            ch,
            CharKey {
                keycode,
                level: Level::AltGr,
            },
        );
        placed_dynamic.insert(ch);
    }

    // The keymap's `maximum` must cover every keycode we define.
    let max_keycode = base
        .iter()
        .map(|b| b.keycode)
        .chain(PERMANENT_ASCII_KEYS.iter().map(|(kc, _, _)| *kc))
        .max()
        .unwrap_or(*SAFE_ALNUM_XKB_KEYCODES.last().unwrap_or(&8));

    let mut keymap = String::new();
    keymap.push_str("xkb_keymap {\n");

    // --- keycodes ---
    keymap.push_str("xkb_keycodes \"whisrs\" {\n");
    keymap.push_str("minimum = 8;\n");
    keymap.push_str(&format!("maximum = {max_keycode};\n"));
    for b in &base {
        keymap.push_str(&format!("<K{}> = {};\n", b.keycode, b.keycode));
    }
    for &(kc, _, _) in PERMANENT_ASCII_KEYS {
        keymap.push_str(&format!("<K{kc}> = {kc};\n"));
    }
    keymap.push_str("};\n");

    // --- types / compatibility: pull in the standard definitions (provides
    // FOUR_LEVEL and the Shift/LevelThree modifier interpretations) ---
    keymap.push_str("xkb_types \"whisrs\" { include \"complete\" };\n");
    keymap.push_str("xkb_compatibility \"whisrs\" { include \"complete\" };\n");

    // --- symbols ---
    keymap.push_str("xkb_symbols \"whisrs\" {\n");
    // Base/modifier keys.
    for b in &base {
        keymap.push_str(&format!(
            "key <K{}> {{ [ {} ] }};\n",
            b.keycode, b.keysym_name
        ));
    }
    // Bind Shift and AltGr so the modifiers we press select levels 1 and 2.
    keymap.push_str(&format!(
        "modifier_map Shift {{ <K{SHIFT_XKB_KEYCODE}> }};\n"
    ));
    keymap.push_str(&format!(
        "modifier_map Mod5 {{ <K{ALTGR_XKB_KEYCODE}> }};\n"
    ));
    // Bind Control so combos (Ctrl+V, Ctrl+Shift+V, ...) sent via
    // `send_combo` register as real Control-held state, not a bare keycode.
    keymap.push_str(&format!(
        "modifier_map Control {{ <K{CTRL_XKB_KEYCODE}> }};\n"
    ));
    // Permanent ASCII keys, each FOUR_LEVEL with an optional dynamic level-2.
    for &(kc, l0, l1) in PERMANENT_ASCII_KEYS {
        let l2 = dynamic_level2
            .get(&kc)
            .map(String::as_str)
            .unwrap_or("NoSymbol");
        keymap.push_str(&format!(
            "key <K{kc}> {{ type=\"FOUR_LEVEL\", [ {l0}, {l1}, {l2} ] }};\n"
        ));
    }
    keymap.push_str("};\n");

    keymap.push_str("};\n");

    (keymap, char_to_key)
}

/// Map the whitespace characters that have no `utf32_to_keysym` keysym
/// (`\n`, `\t`) to the base keycode of their dedicated real key, so they are
/// typed through `Return`/`Tab` rather than silently dropped.
///
/// Returns `Some(evdev_code + 8)` for `\n` (Enter) and `\t` (Tab), and `None`
/// for every other character (which then goes through the dynamic
/// `char_to_keycode` map). Pure and unit-testable without a Wayland
/// connection.
pub(crate) fn special_base_keycode(ch: char) -> Option<u32> {
    use evdev::Key;
    match ch {
        '\n' => Some(u32::from(Key::KEY_ENTER.code()) + 8),
        '\t' => Some(u32::from(Key::KEY_TAB.code()) + 8),
        _ => None,
    }
}

/// Collect the distinct *dynamic non-ASCII* characters in `text`, in first-seen
/// order: characters that are not handled by [`special_base_keycode`], not part
/// of the permanent ASCII layout ([`is_permanent_ascii`]), and that have a
/// non-`NoSymbol` XKB keysym. Characters with no keysym are logged once and
/// skipped (same behaviour as [`build_keymap_string`]).
///
/// Because permanent ASCII is excluded, a stream of pure-ASCII deltas (or
/// deltas that only reuse already-mapped non-ASCII glyphs) yields an empty
/// result — and therefore never triggers a keymap re-upload. This is what makes
/// English-with-occasional-non-Latin streaming re-upload-free, eliminating the
/// keymap-apply race for the common case.
///
/// Factored out so the new-glyph accumulation logic is unit-testable without a
/// Wayland connection.
pub(crate) fn distinct_dynamic_chars(text: &str) -> Vec<char> {
    let mut seen: std::collections::HashSet<char> = std::collections::HashSet::new();
    let mut out: Vec<char> = Vec::new();
    for ch in text.chars() {
        if special_base_keycode(ch).is_some() || is_permanent_ascii(ch) {
            continue;
        }
        if !seen.insert(ch) {
            continue;
        }
        let keysym = xkb::utf32_to_keysym(ch as u32);
        if keysym == xkb::keysyms::KEY_NoSymbol.into() {
            warn!("wayland-vk: no keysym for char {ch:?}; skipping");
            continue;
        }
        out.push(ch);
    }
    out
}

/// State threaded through the Wayland event queue. The two protocol objects
/// (`ZwpVirtualKeyboardManagerV1` / `ZwpVirtualKeyboardV1`) have no events, so
/// their `Dispatch` impls are empty; the only real work happens in the
/// registry/seat handlers (which we also ignore — globals come from the
/// `GlobalList`).
struct State;

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardManagerV1,
        _event: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardV1,
        _event: <ZwpVirtualKeyboardV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// Layout-independent virtual keyboard backed by `zwp_virtual_keyboard_v1`.
///
/// Construct with [`WaylandVkKeyboard::new`]; if the compositor does not
/// advertise the manager global, construction fails with a clear error so the
/// caller can fall back to the uinput [`crate::Keyboard`].
pub struct WaylandVkKeyboard {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    qh: QueueHandle<State>,
    vk: ZwpVirtualKeyboardV1,
    /// Resolved key (keycode + level/modifier) for every character currently
    /// typeable against the uploaded keymap. Always contains the full permanent
    /// ASCII set plus any accumulated dynamic non-ASCII glyphs.
    char_to_key: HashMap<char, CharKey>,
    /// Dynamic *non-ASCII* characters accumulated across `type_text` calls, in
    /// the first-seen order that determines their AltGr-slot assignment.
    /// Rebuilding the keymap from this slice yields stable keycodes, so we only
    /// re-upload when a genuinely new non-ASCII glyph appears.
    ordered_chars: Vec<char>,
    /// `true` once a keymap has been uploaded for the current char set.
    keymap_uploaded: bool,
    /// Inter-event delay (mirrors the uinput backend's delay semantics).
    key_delay: Duration,
    /// Monotonically increasing millisecond timestamp for `key` events. The
    /// protocol only requires a consistent clock with millisecond
    /// granularity and an undefined base, so a simple counter suffices.
    time: u32,
}

impl WaylandVkKeyboard {
    /// Open a Wayland connection and bind the virtual-keyboard manager.
    ///
    /// `key_delay` is the inter-event delay between injected key events
    /// (press/release), matching [`crate::Keyboard`].
    ///
    /// # Errors
    ///
    /// Returns an error if there is no Wayland display, or — importantly for
    /// fallback — if the compositor does not support `zwp_virtual_keyboard_v1`
    /// (`"compositor does not support zwp_virtual_keyboard_v1"`).
    pub fn new(key_delay: Duration) -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env()
            .context("failed to connect to Wayland display (is WAYLAND_DISPLAY set?)")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)
            .context("failed to initialise Wayland registry/globals")?;
        let qh = queue.handle();
        let mut state = State;

        // Bind the seat (version 1 is enough for create_virtual_keyboard).
        let seat: WlSeat = globals
            .bind(&qh, 1..=1, ())
            .context("compositor exposes no wl_seat")?;

        // CAPABILITY PROBE: absence of the manager global means the
        // compositor doesn't implement the protocol — surface a clear error
        // so the daemon can fall back to uinput.
        let manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| anyhow!("compositor does not support zwp_virtual_keyboard_v1"))?;

        let vk = manager.create_virtual_keyboard(&seat, &qh, ());

        // Ensure the protocol objects are created server-side before use.
        queue
            .roundtrip(&mut state)
            .context("initial Wayland roundtrip failed")?;

        debug!("wayland-vk virtual keyboard created");

        Ok(Self {
            conn,
            queue,
            qh,
            vk,
            char_to_key: HashMap::new(),
            ordered_chars: Vec::new(),
            keymap_uploaded: false,
            key_delay,
            time: 0,
        })
    }

    /// Build the keymap for the accumulated non-ASCII `chars`, upload it to the
    /// compositor, and record the resulting `char → CharKey` map.
    ///
    /// # Eliminating the keymap-apply race
    ///
    /// Uploading a keymap and then immediately pressing a key races the
    /// compositor's propagation of the new keymap to the focused client: on
    /// some compositors (notably Hyprland; cf. the analogous labwc
    /// virtual-keyboard re-broadcast bug) the key event can be processed before
    /// the freshly-uploaded keymap is active for key interpretation, so the
    /// keystroke is dropped or misread. We therefore (a) `roundtrip` so the
    /// server has *received and acked* the keymap, and (b) wait a bounded
    /// [`KEYMAP_SETTLE`] so the new keymap is actually live before any key is
    /// emitted.
    ///
    /// With ASCII now permanent, this re-upload+settle happens only when a
    /// genuinely new non-ASCII glyph first appears — rare — so the settle cost
    /// is paid almost never, while pure-ASCII / known-glyph streaming never
    /// re-uploads at all.
    fn upload_keymap(&mut self, chars: &[char]) -> anyhow::Result<()> {
        let (keymap_str, map) = build_keymap_string(chars);

        // Write the keymap (NUL-terminated, as XKB expects) to an fd. Prefer a
        // memfd; the size reported to the compositor includes the trailing
        // NUL.
        let bytes = keymap_str.into_bytes();
        let fd = create_keymap_fd(&bytes)?;
        let size = (bytes.len() + 1) as u32;

        use std::os::fd::AsFd;
        self.vk.keymap(KEYMAP_FORMAT_TEXT_V1, fd.as_fd(), size);

        self.conn.flush().context("flush after keymap upload")?;
        let mut state = State;
        self.queue
            .roundtrip(&mut state)
            .context("roundtrip after keymap upload")?;

        // Bounded settle so the uploaded keymap is live for key interpretation
        // before we emit any key. See the doc comment above.
        std::thread::sleep(KEYMAP_SETTLE);

        self.char_to_key = map;
        self.keymap_uploaded = true;
        Ok(())
    }

    /// Press then release `keycode` once, flushing and sleeping `key_delay`
    /// between each event (mirrors the uinput backend's delay semantics).
    fn tap_keycode(&mut self, keycode: u32) -> anyhow::Result<()> {
        self.emit_key(keycode, KEY_STATE_PRESSED)?;
        self.emit_key(keycode, KEY_STATE_RELEASED)?;
        Ok(())
    }

    /// Tap a character via its [`CharKey`], declaring the modifier its level
    /// requires through `zwp_virtual_keyboard_v1.modifiers` (the **wtype model**)
    /// rather than pressing a physical modifier key.
    ///
    /// # Why the modifier mask, not a key press
    ///
    /// Pressing a modifier *keycode* via `.key` does not reliably update the
    /// modifier state the compositor uses to pick a keymap **level** for the
    /// subsequent key: the propagation of "Shift is down" to the focused
    /// client's key-interpretation state can lag the key press, so the letter
    /// resolves at level 0 (e.g. the Arabic glyph at AltGr-level came out as the
    /// base digit on its keycode — the symptom this fixes). `.modifiers`
    /// declares the depressed-modifier *mask* directly and atomically, which is
    /// exactly how `wtype` produces shifted/`AltGr` symbols. Level 0 needs no
    /// mask; level 1 sets [`SHIFT_MOD_MASK`]; level 2 sets [`ALTGR_MOD_MASK`].
    fn tap_char_key(&mut self, key: CharKey) -> anyhow::Result<()> {
        let mask = match key.level {
            Level::Base => 0,
            Level::Shift => SHIFT_MOD_MASK,
            Level::AltGr => ALTGR_MOD_MASK,
        };
        if mask != 0 {
            self.set_modifiers(mask)?;
        }
        self.tap_keycode(key.keycode)?;
        if mask != 0 {
            self.set_modifiers(0)?;
        }
        Ok(())
    }

    /// Declare the depressed-modifier mask via `zwp_virtual_keyboard_v1.modifiers`
    /// (latched/locked = 0, group 0), then flush and sleep `key_delay` so the
    /// compositor applies it before the next key event.
    fn set_modifiers(&mut self, depressed: u32) -> anyhow::Result<()> {
        self.vk.modifiers(depressed, 0, 0, 0);
        self.conn.flush().context("flush after modifiers")?;
        std::thread::sleep(self.key_delay);
        Ok(())
    }

    /// Emit a single key press or release, then flush and sleep `key_delay`.
    ///
    /// `keycode` is an **XKB** keycode (as stored in `char_to_keycode` and the
    /// `evdev + 8` base-key positions). It is converted to the **evdev** keycode
    /// the protocol expects via [`xkb_to_evdev_keycode`] right before the wire
    /// call, because the compositor re-adds `8` before the keymap lookup. See
    /// [`EVDEV_XKB_OFFSET`] for the authoritative protocol rationale.
    fn emit_key(&mut self, keycode: u32, key_state: u32) -> anyhow::Result<()> {
        let evdev_keycode = xkb_to_evdev_keycode(keycode);
        self.vk.key(self.time, evdev_keycode, key_state);
        self.time = self.time.saturating_add(self.key_delay.as_millis() as u32);
        self.conn.flush().context("flush after key event")?;
        std::thread::sleep(self.key_delay);
        Ok(())
    }

    /// Type `text`, reusing the uploaded keymap whenever possible.
    ///
    /// Printable ASCII (letters, digits, punctuation, space) is **permanent** in
    /// every keymap, so a delta containing only ASCII (or only already-mapped
    /// non-ASCII glyphs) types with **zero** re-uploads — eliminating the
    /// keymap-apply race for the common case. The keymap is re-uploaded only
    /// when a delta introduces a genuinely new *non-ASCII* glyph; those glyphs
    /// accumulate (first-seen order in [`Self::ordered_chars`]) so existing
    /// glyphs keep their AltGr slots and re-uploads become rare. Every re-upload
    /// is synced + settled (see [`Self::upload_keymap`]).
    ///
    /// `\n`/`\t` are typed through their base keys ([`special_base_keycode`])
    /// and never enter the dynamic map.
    ///
    /// # Over-capacity handling (no silent drops)
    ///
    /// If the running set of distinct non-ASCII glyphs would exceed
    /// [`dynamic_capacity`], the oldest glyphs are **evicted** (LRU-style) to
    /// make room — but never glyphs needed by *this* delta. The new accumulated
    /// set is rebuilt to always contain every non-ASCII glyph in the current
    /// text, so a single re-upload makes the whole delta typeable. Only when a
    /// **single** delta itself contains more than `dynamic_capacity` distinct
    /// non-ASCII glyphs does it fall back to the per-batch path — still synced,
    /// still never a silent drop.
    fn type_text_inner(&mut self, text: &str) -> anyhow::Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        // Distinct typeable *non-ASCII* dynamic chars in this text (excludes
        // ASCII, `\n`/`\t`, and chars with no keysym), in first-seen order.
        let wanted = distinct_dynamic_chars(text);
        let capacity = dynamic_capacity();

        // A single delta with more distinct non-ASCII glyphs than fit in one
        // keymap: batch it (rare). ASCII stays permanent throughout.
        if wanted.len() > capacity {
            self.ordered_chars.clear();
            self.char_to_key = permanent_ascii_map();
            self.type_text_batched(text)?;
            self.ordered_chars.clear();
            self.keymap_uploaded = false;
            return Ok(());
        }

        // Which of this delta's glyphs are genuinely new (not already mapped).
        let new_chars: Vec<char> = wanted
            .iter()
            .copied()
            .filter(|ch| !self.char_to_key.contains_key(ch))
            .collect();

        if new_chars.is_empty() {
            // Nothing new: reuse the existing keymap, no re-upload. (If no
            // keymap has ever been uploaded — e.g. the very first delta, or text
            // that is purely ASCII/`\n`/`\t` — upload once so the keys exist.)
            if !self.keymap_uploaded {
                let ordered = self.ordered_chars.clone();
                self.upload_keymap(&ordered)?;
            }
            return self.type_chars(text);
        }

        // Build the next accumulated set. Keep existing glyphs in their stable
        // first-seen order, then append the new ones. If that exceeds capacity,
        // evict the OLDEST glyphs that are NOT needed by this delta (LRU), so
        // the whole current delta always fits and stays resolvable.
        let mut next: Vec<char> = self.ordered_chars.clone();
        next.extend(new_chars.iter().copied());
        if next.len() > capacity {
            let needed: std::collections::HashSet<char> = wanted.iter().copied().collect();
            let overflow = next.len() - capacity;
            let mut to_remove = overflow;
            // Evict oldest-first, skipping glyphs the current delta needs.
            next.retain(|ch| {
                if to_remove > 0 && !needed.contains(ch) {
                    to_remove -= 1;
                    false
                } else {
                    true
                }
            });
            // `wanted.len() <= capacity` guarantees enough evictable slots.
            debug_assert!(next.len() <= capacity);
        }

        self.ordered_chars = next;
        let ordered = self.ordered_chars.clone();
        self.upload_keymap(&ordered)?;
        self.type_chars(text)
    }

    /// Type each char of `text` using the currently-uploaded keymap:
    /// `\n`/`\t` via their base keycodes, everything else via `char_to_key`
    /// (pressing Shift/AltGr as the char's level requires). Assumes the keymap
    /// already covers every dynamic glyph in `text` (the caller guarantees this;
    /// ASCII is always covered).
    fn type_chars(&mut self, text: &str) -> anyhow::Result<()> {
        for ch in text.chars() {
            if let Some(keycode) = special_base_keycode(ch) {
                self.tap_keycode(keycode)?;
                continue;
            }
            let Some(&key) = self.char_to_key.get(&ch) else {
                // Char had no keysym (skipped during keymap build).
                warn!("wayland-vk: cannot type char {ch:?} (no keysym); skipping");
                continue;
            };
            self.tap_char_key(key)?;
        }
        Ok(())
    }

    /// Fallback path for a single call whose distinct *non-ASCII* glyph count
    /// exceeds [`dynamic_capacity`]: split into batches of <= capacity distinct
    /// non-ASCII chars, uploading a fresh (synced + settled) keymap per batch.
    /// ASCII and `\n`/`\t` ride the permanent/base keys present in every keymap,
    /// so they never count against a batch and never cause a split.
    fn type_text_batched(&mut self, text: &str) -> anyhow::Result<()> {
        let capacity = dynamic_capacity();
        let chars: Vec<char> = text.chars().collect();
        let mut idx = 0;
        while idx < chars.len() {
            // Greedily grow a batch until adding the next NEW *non-ASCII* char
            // would exceed capacity. ASCII / `\n` / `\t` don't count.
            let mut distinct: std::collections::HashSet<char> = std::collections::HashSet::new();
            let start = idx;
            while idx < chars.len() {
                let ch = chars[idx];
                let counts = special_base_keycode(ch).is_none() && !is_permanent_ascii(ch);
                if counts && !distinct.contains(&ch) && distinct.len() >= capacity {
                    break;
                }
                if counts {
                    distinct.insert(ch);
                }
                idx += 1;
            }
            let batch = &chars[start..idx];

            // Upload a keymap covering exactly this batch's distinct non-ASCII
            // chars (plus permanent ASCII), then type them.
            let batch_dynamic: Vec<char> = {
                let mut seen = std::collections::HashSet::new();
                batch
                    .iter()
                    .copied()
                    .filter(|&c| {
                        special_base_keycode(c).is_none()
                            && !is_permanent_ascii(c)
                            && seen.insert(c)
                    })
                    .collect()
            };
            self.upload_keymap(&batch_dynamic)?;
            for &ch in batch {
                if let Some(keycode) = special_base_keycode(ch) {
                    self.tap_keycode(keycode)?;
                    continue;
                }
                let Some(&key) = self.char_to_key.get(&ch) else {
                    // Char had no keysym (skipped during keymap build).
                    warn!("wayland-vk: cannot type char {ch:?} (no keysym); skipping");
                    continue;
                };
                self.tap_char_key(key)?;
            }
        }

        Ok(())
    }
}

impl KeyInjector for WaylandVkKeyboard {
    fn type_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.type_text_inner(text)
    }

    fn backspace(&mut self, count: usize) -> anyhow::Result<()> {
        // Ensure a keymap is present so the BackSpace base key resolves.
        if !self.keymap_uploaded {
            self.upload_keymap(&[])?;
        }
        let backspace = u32::from(evdev::Key::KEY_BACKSPACE.code()) + 8;
        for _ in 0..count {
            self.tap_keycode(backspace)?;
        }
        Ok(())
    }

    /// Send a key combo (e.g. Ctrl+V, Ctrl+Shift+V).
    ///
    /// Recognized modifier keys (Ctrl, Shift) are declared via
    /// `zwp_virtual_keyboard_v1.modifiers` — the same "wtype model" atomic
    /// mask declaration [`Self::tap_char_key`] uses — rather than pressed as
    /// keycodes. Pressing a modifier keycode directly is not sufficient here:
    /// our uploaded keymap only makes a key a *real* modifier via
    /// `modifier_map`, and even then, propagating "held" state from a raw key
    /// press to the level/combo interpretation the focused client uses can
    /// lag the press (see [`Self::tap_char_key`]'s doc comment). Declaring the
    /// mask directly is atomic and reliable. Any other keys in `keys` (e.g.
    /// `KEY_V`) are tapped while the mask is held, in the order given.
    fn send_combo(&mut self, keys: &[evdev::Key]) -> anyhow::Result<()> {
        if !self.keymap_uploaded {
            self.upload_keymap(&[])?;
        }

        use evdev::Key;
        let mut mask = 0u32;
        let mut real_keys = Vec::with_capacity(keys.len());
        for &key in keys {
            match key {
                Key::KEY_LEFTCTRL | Key::KEY_RIGHTCTRL => mask |= CTRL_MOD_MASK,
                Key::KEY_LEFTSHIFT | Key::KEY_RIGHTSHIFT => mask |= SHIFT_MOD_MASK,
                other => real_keys.push(other),
            }
        }

        if mask != 0 {
            self.set_modifiers(mask)?;
        }

        // Press the non-modifier keys (evdev code + 8) in order, release in
        // reverse.
        for key in &real_keys {
            let keycode = u32::from(key.code()) + 8;
            self.emit_key(keycode, KEY_STATE_PRESSED)?;
        }
        for key in real_keys.iter().rev() {
            let keycode = u32::from(key.code()) + 8;
            self.emit_key(keycode, KEY_STATE_RELEASED)?;
        }

        if mask != 0 {
            self.set_modifiers(0)?;
        }

        Ok(())
    }

    fn set_key_delay(&mut self, delay: Duration) {
        self.key_delay = delay;
    }
}

impl Drop for WaylandVkKeyboard {
    fn drop(&mut self) {
        // Best-effort: tell the compositor to drop the virtual keyboard.
        self.vk.destroy();
        let _ = self.conn.flush();
        let _ = self.qh; // keep field used without warnings on all cfgs
    }
}

/// Create a writable, sealed-capable fd containing `bytes` followed by a
/// trailing NUL byte (XKB keymap strings must be NUL-terminated when mapped
/// from an fd).
///
/// Uses a `memfd` via rustix. The returned fd is positioned so the compositor
/// can `mmap` the whole region.
fn create_keymap_fd(bytes: &[u8]) -> anyhow::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{memfd_create, MemfdFlags};

    let fd = memfd_create("whisrs-keymap", MemfdFlags::CLOEXEC)
        .context("memfd_create for keymap failed")?;

    // Write through a File wrapper so we get buffered std::io semantics; the
    // File takes ownership, so reclaim the fd afterwards.
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes).context("writing keymap to memfd")?;
    file.write_all(&[0])
        .context("writing keymap NUL terminator")?;
    file.flush().ok();
    let fd: std::os::fd::OwnedFd = file.into();
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile a keymap string into an `xkb::Keymap`, panicking with the source
    /// on failure (the source is invaluable when an xkb syntax error slips in).
    fn compile(keymap_str: &str) -> xkb::Keymap {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_string(
            &ctx,
            keymap_str.to_string(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap_or_else(|| panic!("keymap failed to compile:\n{keymap_str}"))
    }

    /// Resolve the glyph the *compositor* would produce for a [`CharKey`] under
    /// our uploaded keymap, simulating exactly what production does: declare the
    /// level's modifier **mask** via `update_mask` (the `zwp_virtual_keyboard_v1
    /// .modifiers` path), look up the key, then clear the mask. Returns the
    /// decoded UTF-8 string (as `xkb_state_key_get_utf8` would yield).
    fn resolve_via_state(keymap: &xkb::Keymap, key: CharKey) -> String {
        let mut state = xkb::State::new(keymap);
        let mask = match key.level {
            Level::Base => 0,
            Level::Shift => SHIFT_MOD_MASK,
            Level::AltGr => ALTGR_MOD_MASK,
        };
        state.update_mask(mask, 0, 0, 0, 0, 0);
        let out = state.key_get_utf8(xkb::Keycode::new(key.keycode));
        state.update_mask(0, 0, 0, 0, 0, 0);
        out
    }

    /// For every char in `input`, assert that the compositor (via xkb state with
    /// the correct modifier) reproduces it exactly from the built keymap. This
    /// is the end-to-end correctness property: zero garble, zero drops, with the
    /// Shift/AltGr modifier our typing code emits.
    fn assert_roundtrip(input: &str) {
        let dynamic = distinct_dynamic_chars(input);
        let (keymap_str, map) = build_keymap_string(&dynamic);
        let keymap = compile(&keymap_str);

        let mut checked = 0usize;
        for ch in input.chars() {
            if special_base_keycode(ch).is_some() {
                continue; // \n / \t ride base keys, tested separately
            }
            let key = *map
                .get(&ch)
                .unwrap_or_else(|| panic!("char {ch:?} was not assigned a CharKey"));
            let got = resolve_via_state(&keymap, key);
            assert_eq!(
                got,
                ch.to_string(),
                "char {ch:?} resolved to {got:?} at {key:?} (compositor-sim mismatch)"
            );
            checked += 1;
        }
        eprintln!(
            "roundtrip OK: {checked} chars for {input:?} (dynamic glyphs: {})",
            dynamic.len()
        );
    }

    #[test]
    fn roundtrip_mixed_scripts() {
        // Latin (mixed case) + Arabic + Greek + Cyrillic + punctuation/digits.
        let input = "Ammeter اميتر ω д 1+2=3! UPPER lower";
        assert_roundtrip(input);
        // The non-ASCII dynamic count is comfortably within one keymap.
        let dynamic = distinct_dynamic_chars(input);
        assert!(dynamic.len() <= dynamic_capacity());
        assert!(
            dynamic.iter().all(|c| (*c as u32) >= 0x80),
            "distinct_dynamic_chars must contain only non-ASCII, got {dynamic:?}"
        );
    }

    /// EVERY printable ASCII character (0x20..=0x7E) must be permanent and
    /// resolvable from a keymap built with ZERO dynamic glyphs — proving ASCII
    /// never depends on a re-upload. This is the heart of the robustness fix.
    #[test]
    fn all_printable_ascii_is_permanent_and_resolvable() {
        let (keymap_str, map) = build_keymap_string(&[]);
        let keymap = compile(&keymap_str);

        let mut checked = 0usize;
        for cp in 0x20u32..=0x7e {
            let ch = char::from_u32(cp).unwrap();
            assert!(
                is_permanent_ascii(ch),
                "printable ASCII {ch:?} (U+{cp:04X}) must be permanent"
            );
            let key = *map
                .get(&ch)
                .unwrap_or_else(|| panic!("permanent ASCII {ch:?} missing from empty-keymap map"));
            let got = resolve_via_state(&keymap, key);
            assert_eq!(
                got,
                ch.to_string(),
                "permanent ASCII {ch:?} resolved to {got:?} at {key:?}"
            );
            checked += 1;
        }
        assert_eq!(checked, 95, "expected all 95 printable ASCII chars");
        eprintln!("ascii-permanence OK: all 95 printable ASCII resolvable with zero dynamics");
    }

    /// The streaming/re-upload regression test, fully offline and deterministic.
    ///
    /// Simulates the daemon's streaming accumulation: feed many small deltas
    /// (mixed EN + non-Latin) one at a time. After EACH delta, rebuild the
    /// keymap from the accumulated non-ASCII glyphs and assert that EVERY
    /// character typed so far — all permanent ASCII PLUS every previously-seen
    /// non-ASCII glyph — still resolves to itself. This is exactly the property
    /// the intermittent bug violated: a freshly-introduced glyph or a space
    /// becoming unresolvable across a re-upload.
    #[test]
    fn streaming_accumulation_keeps_all_chars_resolvable() {
        // EN + AR + RU interleaved, as a streaming transcription might deliver.
        let deltas = [
            "the quick ",
            "brown اهلا ",
            "fox مرحبا ",
            "привет jumps ",
            "over мир ",
            "the lazy ",
            "صباح dog ",
            "доброе утро!",
        ];

        let mut ordered: Vec<char> = Vec::new(); // accumulated non-ASCII
        let mut typed_so_far = String::new();

        for delta in deltas {
            // Accumulate new non-ASCII glyphs (mirrors type_text_inner).
            for ch in distinct_dynamic_chars(delta) {
                if !ordered.contains(&ch) {
                    ordered.push(ch);
                }
            }
            typed_so_far.push_str(delta);

            assert!(
                ordered.len() <= dynamic_capacity(),
                "test fixture exceeded capacity ({} > {})",
                ordered.len(),
                dynamic_capacity()
            );

            let (keymap_str, map) = build_keymap_string(&ordered);
            let keymap = compile(&keymap_str);

            // Every char typed so far must resolve — ASCII (permanent) and every
            // accumulated non-ASCII glyph alike.
            for ch in typed_so_far.chars() {
                if special_base_keycode(ch).is_some() {
                    continue;
                }
                let key = *map.get(&ch).unwrap_or_else(|| {
                    panic!("after delta {delta:?}: char {ch:?} lost from keymap")
                });
                let got = resolve_via_state(&keymap, key);
                assert_eq!(
                    got,
                    ch.to_string(),
                    "after delta {delta:?}: char {ch:?} no longer resolves ({got:?} at {key:?})"
                );
            }
        }
        eprintln!(
            "streaming-accumulation OK: {} deltas, {} accumulated non-ASCII glyphs, all chars \
             stayed resolvable across every re-upload",
            deltas.len(),
            ordered.len()
        );
    }

    /// Existing glyphs must keep a STABLE CharKey as new glyphs accumulate, so a
    /// re-upload never relocates a previously-typed character (which would be a
    /// silent garble if a key event raced the relocation).
    #[test]
    fn accumulation_is_stable_across_reuploads() {
        let (_k1, map1) = build_keymap_string(&distinct_dynamic_chars("اهلا"));
        let m1 = *map1.get(&'ا').unwrap();

        // Add more non-ASCII glyphs; the original 'ا' must keep its CharKey.
        let (_k2, map2) = build_keymap_string(&distinct_dynamic_chars("اهلا مرحبا привет"));
        let m2 = *map2.get(&'ا').unwrap();
        assert_eq!(
            m1, m2,
            "glyph 'ا' relocated across re-upload: {m1:?} -> {m2:?}"
        );

        // ASCII keys are identical in both (permanent).
        assert_eq!(map1.get(&'a'), map2.get(&'a'));
        assert_eq!(map1.get(&' '), map2.get(&' '));
    }

    #[test]
    fn roundtrip_batches_over_capacity() {
        // Strictly more distinct *non-ASCII* glyphs than fit in one keymap, to
        // exercise the batching path. Roundtrip each batch independently.
        let needed = dynamic_capacity() + 20;
        let mut input = String::new();
        // Arabic + Cyrillic + CJK + Greek give a wide non-ASCII spread.
        let mut cp = 0x0410u32; // Cyrillic А
        let mut count = 0;
        while count < needed {
            if let Some(c) = char::from_u32(cp) {
                if !c.is_control()
                    && (c as u32) >= 0x80
                    && xkb::utf32_to_keysym(cp) != xkb::keysyms::KEY_NoSymbol.into()
                {
                    input.push(c);
                    count += 1;
                }
            }
            cp += 1;
        }
        let dynamic = distinct_dynamic_chars(&input);
        assert!(dynamic.len() > dynamic_capacity());

        // Batch like type_text_batched and roundtrip each batch's dynamics.
        let mut idx = 0;
        let mut total = 0usize;
        let mut batches = 0usize;
        while idx < dynamic.len() {
            let end = (idx + dynamic_capacity()).min(dynamic.len());
            let batch = &dynamic[idx..end];
            let (keymap_str, map) = build_keymap_string(batch);
            let keymap = compile(&keymap_str);
            for &ch in batch {
                let key = *map.get(&ch).expect("batch char has CharKey");
                assert_eq!(resolve_via_state(&keymap, key), ch.to_string());
                total += 1;
            }
            batches += 1;
            idx = end;
        }
        assert_eq!(total, dynamic.len());
        assert!(batches >= 2, "expected >=2 batches, got {batches}");
        eprintln!(
            "batching OK: {} non-ASCII glyphs across {batches} batches (capacity {})",
            dynamic.len(),
            dynamic_capacity()
        );
    }

    #[test]
    fn special_base_keycode_maps_newline_and_tab() {
        use evdev::Key;
        assert_eq!(
            special_base_keycode('\n'),
            Some(u32::from(Key::KEY_ENTER.code()) + 8)
        );
        assert_eq!(
            special_base_keycode('\t'),
            Some(u32::from(Key::KEY_TAB.code()) + 8)
        );
        assert_eq!(special_base_keycode('a'), None);
        assert_eq!(special_base_keycode(' '), None);
        assert_eq!(special_base_keycode('م'), None);
    }

    #[test]
    fn newline_tab_are_not_dynamic_but_compile_at_base_keycodes() {
        use evdev::Key;

        // \n / \t are filtered before keymap build (ride Return/Tab base keys).
        let dynamic = distinct_dynamic_chars("\n\t");
        assert!(dynamic.is_empty(), "got {dynamic:?}");
        let (keymap_str, _map) = build_keymap_string(&dynamic);
        let keymap = compile(&keymap_str);

        let enter_kc = u32::from(Key::KEY_ENTER.code()) + 8;
        let enter_syms = keymap.key_get_syms_by_level(xkb::Keycode::new(enter_kc), 0, 0);
        assert_eq!(enter_syms.len(), 1);
        assert_eq!(xkb::keysym_get_name(enter_syms[0]), "Return");

        let tab_kc = u32::from(Key::KEY_TAB.code()) + 8;
        let tab_syms = keymap.key_get_syms_by_level(xkb::Keycode::new(tab_kc), 0, 0);
        assert_eq!(tab_syms.len(), 1);
        assert_eq!(xkb::keysym_get_name(tab_syms[0]), "Tab");
    }

    #[test]
    fn distinct_dynamic_chars_excludes_ascii_and_whitespace_base_keys() {
        // ASCII (incl. space) and \n/\t are NOT dynamic; only non-ASCII is.
        let got = distinct_dynamic_chars("a\tb\n ab مرحبا مرحبا");
        // Only the distinct Arabic letters, in first-seen order.
        assert_eq!(got, vec!['م', 'ر', 'ح', 'ب', 'ا']);
    }

    #[test]
    fn accumulation_detects_no_new_chars_on_reused_or_ascii_glyphs() {
        // Build accumulating "مرحبا"; reusing it (reordered) or adding ASCII
        // introduces NO new dynamic glyph -> no re-upload.
        let (_keymap, char_to_key) = build_keymap_string(&distinct_dynamic_chars("مرحبا"));

        for reuse in [" احبرم", "hello world مرحبا", "ABC 123 !@# مرم"] {
            let new_chars: Vec<char> = distinct_dynamic_chars(reuse)
                .into_iter()
                .filter(|ch| !char_to_key.contains_key(ch))
                .collect();
            assert!(
                new_chars.is_empty(),
                "reuse {reuse:?} should introduce no new dynamic glyph, got {new_chars:?}"
            );
        }

        // A genuinely new non-ASCII glyph IS detected.
        let new_chars: Vec<char> = distinct_dynamic_chars("مرحبا привет")
            .into_iter()
            .filter(|ch| !char_to_key.contains_key(ch))
            .collect();
        assert_eq!(new_chars, vec!['п', 'р', 'и', 'в', 'е', 'т']);
    }

    /// The evdev keycodes that must NEVER be emitted as a glyph-carrying key,
    /// because in a standard keymap they resolve to special keysyms compositors
    /// commonly bind (F13–F24, media, `Print`, system keys).
    fn is_dangerous_evdev_keycode(evdev: u32) -> bool {
        if (183..=247).contains(&evdev) {
            return true;
        }
        matches!(
            evdev,
            99 | 113 | 114 | 115 | 139 | 148 | 149 | 163 | 164 | 165
        )
    }

    /// OFFLINE GUARD: no keycode the backend ever puts on the wire as a printable
    /// key (permanent ASCII keys OR dynamic-glyph keycodes) falls in a dangerous
    /// range, and the dynamic keycodes all come from the safe alphanumeric block.
    /// This is what prevented the screenshot-key regression; it must stay green.
    #[test]
    fn no_emitted_keycode_is_a_special_key() {
        let text = "Hello, мир! اهلا ω 1234567890 -=[];'/.,";
        let dynamic = distinct_dynamic_chars(text);
        let (_keymap_str, char_to_key) = build_keymap_string(&dynamic);
        assert!(!char_to_key.is_empty());

        // Every printable key's wire (evdev) keycode is safe.
        for (&ch, key) in &char_to_key {
            let evdev = xkb_to_evdev_keycode(key.keycode);
            assert!(
                !is_dangerous_evdev_keycode(evdev),
                "char {ch:?}: emitted evdev keycode {evdev} (XKB {}) is a special key",
                key.keycode
            );
            // Dynamic (AltGr-level) glyphs must come from the safe block.
            if key.level == Level::AltGr {
                assert!(
                    SAFE_ALNUM_XKB_KEYCODES.contains(&key.keycode),
                    "char {ch:?}: dynamic XKB keycode {} outside safe alphanumeric block",
                    key.keycode
                );
            }
            // Permanent ASCII keys come from PERMANENT_ASCII_KEYS.
            assert!(
                PERMANENT_ASCII_KEYS
                    .iter()
                    .any(|(kc, _, _)| *kc == key.keycode),
                "char {ch:?}: keycode {} is not a permanent-ASCII keycode",
                key.keycode
            );
        }

        // Every base/modifier key we emit is also safe.
        use evdev::Key;
        for key in [
            Key::KEY_BACKSPACE,
            Key::KEY_LEFTCTRL,
            Key::KEY_RIGHTCTRL,
            Key::KEY_LEFTSHIFT,
            Key::KEY_LEFTALT,
            Key::KEY_RIGHTALT,
            Key::KEY_ENTER,
            Key::KEY_TAB,
        ] {
            let evdev = u32::from(key.code());
            assert!(
                !is_dangerous_evdev_keycode(evdev),
                "base key {key:?}: evdev keycode {evdev} is a special key"
            );
        }

        // Discrimination sanity: the OLD high-block scheme must trip the guard.
        assert!(is_dangerous_evdev_keycode(200u32 - EVDEV_XKB_OFFSET)); // 192 / F22
        assert!(is_dangerous_evdev_keycode(210)); // KEY_PRINT
        eprintln!(
            "no-dangerous-keycode OK: {} printable glyphs + base keys all in safe evdev space",
            char_to_key.len()
        );
    }

    /// Bind-safety: in a STOCK `us` keymap (what Hyprland evaluates binds
    /// against), every emitted (keycode, level) resolves to a plain printable
    /// keysym — never `Print`/F-key/media/modifier. Covers level 0 (base),
    /// level 1 (Shift), and level 2 (AltGr) for the permanent ASCII keys, and
    /// level 0/1 for the out-of-block grave/backslash/space keys.
    #[test]
    fn emitted_combos_are_plain_keys_in_stock_keymap() {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let stock = xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("stock us keymap compiles");

        // Helper: in stock us, the keysym at (keycode, level) must be a plain
        // printable char (non-zero, non-control Unicode) — or, for the AltGr
        // level on the safe block, at minimum NOT a dangerous special key.
        let check_plain = |kc: u32, level: u32, what: &str| {
            let syms = stock.key_get_syms_by_level(xkb::Keycode::new(kc), 0, level);
            // Some stock us keys have no AltGr (level 2) symbol — that is fine
            // and safe (NoSymbol can't match a keysym bind).
            if syms.is_empty() {
                return;
            }
            let cp = xkb::keysym_to_utf32(syms[0]);
            if cp == 0 {
                // No Unicode mapping. Only acceptable if it is not a bound
                // special key; for our safe block AltGr level this is NoSymbol
                // or a plain dead key in some locales — accept non-dangerous.
                let name = xkb::keysym_get_name(syms[0]);
                assert!(
                    !name.starts_with("XF86")
                        && !name.starts_with('F')
                        && name != "Print"
                        && name != "Sys_Req",
                    "{what}: keycode {kc} level {level} resolves to special keysym {name} in \
                     stock us"
                );
                return;
            }
            let c = char::from_u32(cp).unwrap_or('\u{0}');
            assert!(
                !c.is_control(),
                "{what}: keycode {kc} level {level} resolves to control char in stock us"
            );
        };

        // Level 0 + level 1 (Shift) for ALL permanent ASCII keys.
        for &(kc, _, _) in PERMANENT_ASCII_KEYS {
            check_plain(kc, 0, "permanent-ascii level0");
            check_plain(kc, 1, "permanent-ascii level1 (Shift)");
        }
        // Level 2 (AltGr) for the safe block (the slots dynamics land on).
        for kc in dynamic_keycode_pool() {
            check_plain(kc, 2, "dynamic AltGr level2");
        }
        eprintln!(
            "stock-keymap bind-safety OK: level0/level1 of {} permanent ASCII keys and AltGr \
             level2 of {} safe keycodes are all plain/non-dangerous in stock us",
            PERMANENT_ASCII_KEYS.len(),
            dynamic_keycode_pool().len()
        );
    }

    /// REGRESSION GUARD for the off-by-8 wire arithmetic: for every glyph, the
    /// evdev keycode handed to `vk.key` + the compositor's +8 must equal the XKB
    /// keycode the keymap defines the glyph at, and the compositor lookup at the
    /// glyph's level (with its modifier) must reproduce the glyph.
    #[test]
    fn vk_key_arg_plus_8_resolves_to_intended_glyph() {
        let text = "ab AB م ω д 1+2=3 !@#";
        let dynamic = distinct_dynamic_chars(text);
        let (keymap_str, char_to_key) = build_keymap_string(&dynamic);
        let keymap = compile(&keymap_str);
        assert!(!char_to_key.is_empty());

        for ch in text.chars() {
            if special_base_keycode(ch).is_some() || ch == ' ' {
                // space is permanent ASCII; covered, but skip the explicit ' '
                // assertion noise here.
            }
            let Some(key) = char_to_key.get(&ch).copied() else {
                continue;
            };
            let evdev = xkb_to_evdev_keycode(key.keycode);
            assert_eq!(
                evdev + EVDEV_XKB_OFFSET,
                key.keycode,
                "char {ch:?}: vk.key arg {evdev} + {EVDEV_XKB_OFFSET} must equal XKB {}",
                key.keycode
            );
            // Compositor-sim end to end: send evdev, it adds 8, looks up at the
            // level the modifier selects -> intended glyph.
            let got = resolve_via_state(&keymap, key);
            assert_eq!(
                got,
                ch.to_string(),
                "char {ch:?} compositor-sim mismatch at {key:?}"
            );
        }

        // Base keys (BackSpace/Return/Tab/Ctrl + the modifiers) still survive.
        use evdev::Key;
        for (key, expected_name) in [
            (Key::KEY_BACKSPACE, "BackSpace"),
            (Key::KEY_ENTER, "Return"),
            (Key::KEY_TAB, "Tab"),
            (Key::KEY_LEFTCTRL, "Control_L"),
            (Key::KEY_RIGHTALT, "ISO_Level3_Shift"),
        ] {
            let xkb_keycode = u32::from(key.code()) + EVDEV_XKB_OFFSET;
            let syms = keymap.key_get_syms_by_level(xkb::Keycode::new(xkb_keycode), 0, 0);
            assert_eq!(syms.len(), 1, "base key {expected_name} keysym count");
            assert_eq!(xkb::keysym_get_name(syms[0]), expected_name);
        }
        eprintln!(
            "keycode-arithmetic OK: vk.key arg + {EVDEV_XKB_OFFSET} == XKB keycode for all glyphs"
        );
    }

    #[test]
    fn keysym_name_roundtrip_tricky_chars() {
        // Dynamic (non-ASCII) tricky glyphs round-trip through their AltGr slot.
        for &ch in &['é', 'م', '😀', 'ω', 'д'] {
            let (keymap_str, map) = build_keymap_string(&[ch]);
            let keymap = compile(&keymap_str);
            let key = *map
                .get(&ch)
                .unwrap_or_else(|| panic!("char {ch:?} not assigned a CharKey"));
            assert_eq!(
                key.level,
                Level::AltGr,
                "non-ASCII {ch:?} should be AltGr-level"
            );
            assert_eq!(resolve_via_state(&keymap, key), ch.to_string());
        }
    }

    /// Uppercase letters and shifted punctuation resolve at level 1 (Shift), and
    /// the same physical keycode at level 0 yields the lowercase/base symbol —
    /// proving the multi-level scheme is wired correctly.
    #[test]
    fn shifted_ascii_uses_shift_level() {
        let map = permanent_ascii_map();
        // Uppercase 'A' is Shift of 'a' on the same keycode.
        let lower = map[&'a'];
        let upper = map[&'A'];
        assert_eq!(lower.keycode, upper.keycode, "'a'/'A' share a keycode");
        assert_eq!(lower.level, Level::Base);
        assert_eq!(upper.level, Level::Shift);

        // Shifted punctuation: '!' is Shift of '1'.
        let one = map[&'1'];
        let bang = map[&'!'];
        assert_eq!(one.keycode, bang.keycode);
        assert_eq!(bang.level, Level::Shift);

        // Verify against a compiled keymap with no dynamics.
        let (keymap_str, m) = build_keymap_string(&[]);
        let keymap = compile(&keymap_str);
        assert_eq!(resolve_via_state(&keymap, m[&'A']), "A");
        assert_eq!(resolve_via_state(&keymap, m[&'a']), "a");
        assert_eq!(resolve_via_state(&keymap, m[&'!']), "!");
        assert_eq!(resolve_via_state(&keymap, m[&'1']), "1");
        assert_eq!(resolve_via_state(&keymap, m[&' ']), " ");
    }

    /// Space — the prime drop victim in the bug — is a permanent base-level key
    /// (never dynamic), so it can never be lost to a re-upload race.
    #[test]
    fn space_is_permanent_base_level() {
        assert!(is_permanent_ascii(' '));
        let key = permanent_ascii_map()[&' '];
        assert_eq!(
            key.level,
            Level::Base,
            "space must be base-level (no modifier)"
        );
        // And it resolves from a zero-dynamic keymap.
        let (keymap_str, m) = build_keymap_string(&[]);
        let keymap = compile(&keymap_str);
        assert_eq!(resolve_via_state(&keymap, m[&' ']), " ");
    }

    /// The mod-mask constants we send via `zwp_virtual_keyboard_v1.modifiers`
    /// must match the REAL modifier indices the compiled keymap assigns to Shift
    /// and Mod5 (AltGr) — otherwise a future keymap change could silently send
    /// the wrong mask. Verifies that `update_mask(SHIFT_MOD_MASK)` selects the
    /// Shift symbol and `update_mask(ALTGR_MOD_MASK)` selects the AltGr glyph,
    /// using the actual built keymap (with one dynamic glyph at AltGr).
    #[test]
    fn modifier_masks_select_levels() {
        let (keymap_str, map) = build_keymap_string(&['م']);
        let keymap = compile(&keymap_str);

        // The keymap's real modifier indices must line up with our masks.
        assert_eq!(
            keymap.mod_get_index(xkb::MOD_NAME_SHIFT),
            SHIFT_MOD_MASK.trailing_zeros(),
            "Shift modifier index mismatch with SHIFT_MOD_MASK"
        );
        assert_eq!(
            keymap.mod_get_index("Mod5"),
            ALTGR_MOD_MASK.trailing_zeros(),
            "Mod5/AltGr modifier index mismatch with ALTGR_MOD_MASK"
        );
        assert_eq!(
            keymap.mod_get_index(xkb::MOD_NAME_CTRL),
            CTRL_MOD_MASK.trailing_zeros(),
            "Control modifier index mismatch with CTRL_MOD_MASK"
        );

        // Drive a single keycode through all three masks and confirm the level.
        let a = map[&'a'];
        let mut st = xkb::State::new(&keymap);

        st.update_mask(0, 0, 0, 0, 0, 0);
        assert_eq!(st.key_get_utf8(xkb::Keycode::new(a.keycode)), "a");

        st.update_mask(SHIFT_MOD_MASK, 0, 0, 0, 0, 0);
        assert_eq!(
            st.key_get_utf8(xkb::Keycode::new(a.keycode)),
            "A",
            "SHIFT_MOD_MASK must select the Shift level"
        );

        // The same physical keycode 'a' lives on (xkb 38) — confirm AltGr there
        // gives the dynamic glyph placed at its AltGr slot, if 'م' landed on 38;
        // otherwise just confirm 'م' resolves via its own CharKey + AltGr mask.
        let meem = map[&'م'];
        assert_eq!(meem.level, Level::AltGr);
        st.update_mask(ALTGR_MOD_MASK, 0, 0, 0, 0, 0);
        assert_eq!(
            st.key_get_utf8(xkb::Keycode::new(meem.keycode)),
            "م",
            "ALTGR_MOD_MASK must select the AltGr (dynamic) level"
        );
        eprintln!(
            "modifier-mask OK: Shift={SHIFT_MOD_MASK} Mod5={ALTGR_MOD_MASK} select levels 1/2, \
             Control={CTRL_MOD_MASK} is a real modifier"
        );
    }
}
