use std::sync::{Mutex as StdMutex, OnceLock};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use whisrs::InjectorBackend;

static KEYBOARD: OnceLock<StdMutex<Option<Box<dyn xkb_type::KeyInjector>>>> = OnceLock::new();

/// Type text at the cursor using uinput (keyboard injection) or clipboard paste.
pub(crate) fn type_text_at_cursor(
    text: &str,
    key_delay: std::time::Duration,
    backend: InjectorBackend,
) -> Result<()> {
    let keyboard_slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
    let mut keyboard_guard = keyboard_slot
        .lock()
        .map_err(|_| anyhow::anyhow!("keyboard mutex poisoned"))?;

    if keyboard_guard.is_none() {
        // Runtime path: short settle delay. The startup prewarm in
        // `warm_keyboard` is what gives X11 time to attach its keymap;
        // we don't want every error-recovery to stall the user's typing
        // path for 1s.
        *keyboard_guard = Some(new_keyboard(
            key_delay, /* prewarm = */ false, backend,
        )?);
    }

    let keyboard = keyboard_guard
        .as_mut()
        .expect("keyboard exists after initialization");
    keyboard.set_key_delay(key_delay);

    let result = keyboard.type_text(text).context("failed to type text");
    if result.is_err() {
        *keyboard_guard = None;
    }
    result?;
    Ok(())
}

/// Send a paste keystroke — Ctrl+V, or Ctrl+Shift+V in terminals — via the
/// **persistent** virtual keyboard (the same device `type_text_at_cursor`
/// uses).
///
/// Must NOT use a fresh per-call uinput device: on some compositors (e.g.
/// KWin) keystrokes from a device the compositor hasn't finished enumerating
/// are dropped, so the paste silently no-ops. The persistent device is already
/// recognized, so its keystrokes land. The combo is raw keycodes (`KEY_V` is
/// `v` in every common layout), so it stays layout-independent.
fn paste_via_keyboard(
    is_terminal: bool,
    key_delay: std::time::Duration,
    backend: InjectorBackend,
) -> Result<()> {
    use evdev::Key;

    let keyboard_slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
    let mut keyboard_guard = keyboard_slot
        .lock()
        .map_err(|_| anyhow::anyhow!("keyboard mutex poisoned"))?;

    if keyboard_guard.is_none() {
        *keyboard_guard = Some(new_keyboard(
            key_delay, /* prewarm = */ false, backend,
        )?);
    }

    let keyboard = keyboard_guard
        .as_mut()
        .expect("keyboard exists after initialization");
    keyboard.set_key_delay(key_delay);

    let combo: &[Key] = if is_terminal {
        &[Key::KEY_LEFTCTRL, Key::KEY_LEFTSHIFT, Key::KEY_V]
    } else {
        &[Key::KEY_LEFTCTRL, Key::KEY_V]
    };

    let result = keyboard
        .send_combo(combo)
        .context("failed to send paste combo");
    if result.is_err() {
        *keyboard_guard = None;
    }
    result
}

/// Clear the current shell prompt line by sending Ctrl+A ("move to start of
/// line") then Ctrl+K ("kill to end of line") via the **persistent** virtual
/// keyboard (the same device `type_text_at_cursor` and `paste_via_keyboard`
/// use). That readline / zle / fish editing pair empties the line in bash, zsh
/// and fish alike.
///
/// Only ever called for terminals. A terminal's mouse highlight is a visual
/// overlay, not an editable selection, so injecting at the cursor would append
/// to the existing line rather than replace it. Clearing the line first makes
/// the injected text the whole line, which is what a "rewrite my selection"
/// command means at a prompt. In GUI text widgets no clear is needed (or
/// wanted): typing/pasting over a real selection replaces it natively.
///
/// Must NOT use a fresh per-call uinput device: on some compositors (e.g.
/// KWin) keystrokes from a device the compositor hasn't finished enumerating
/// are dropped, so the clear silently no-ops. The persistent device is already
/// recognized, so its keystrokes land.
pub(crate) fn clear_line_via_keyboard(
    key_delay: std::time::Duration,
    backend: InjectorBackend,
) -> Result<()> {
    use evdev::Key;

    let keyboard_slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
    let mut keyboard_guard = keyboard_slot
        .lock()
        .map_err(|_| anyhow::anyhow!("keyboard mutex poisoned"))?;

    if keyboard_guard.is_none() {
        *keyboard_guard = Some(new_keyboard(
            key_delay, /* prewarm = */ false, backend,
        )?);
    }

    let keyboard = keyboard_guard
        .as_mut()
        .expect("keyboard exists after initialization");
    keyboard.set_key_delay(key_delay);

    let result = keyboard
        .send_combo(&[Key::KEY_LEFTCTRL, Key::KEY_A])
        .context("failed to send Ctrl+A (move to start of line)")
        .and_then(|()| {
            keyboard
                .send_combo(&[Key::KEY_LEFTCTRL, Key::KEY_K])
                .context("failed to send Ctrl+K (kill to end of line)")
        });
    if result.is_err() {
        *keyboard_guard = None;
    }
    result
}

/// Inject `text` at the cursor, choosing keystrokes or clipboard paste.
///
/// With `paste = false` (default) this types via the virtual keyboard. With
/// `paste = true` it sets the clipboard, sends Ctrl+V (Ctrl+Shift+V for
/// terminals), then restores the previous clipboard — layout-independent
/// injection for compositors that lack the Wayland virtual-keyboard protocol
/// (see [`whisrs::InputConfig::paste`]). Runs in a blocking context (callers
/// wrap it in `spawn_blocking`), so the sleeps/restore use std threads.
///
/// `ClipboardBackend` is text-only, so a clipboard holding non-text content
/// (an image, a file list, ...) can't be captured and round-tripped at all.
/// If the pre-paste read fails, this falls back to typing instead of pasting
/// — overwriting the clipboard via `set_text` first and only then discovering
/// there's nothing valid to restore would destroy that content permanently
/// (see #69), which skipping the *restore* alone can't undo since the damage
/// already happened at `set_text`.
///
/// The restore is otherwise skipped, rather than clobbering the clipboard,
/// when the clipboard no longer holds the text we set — something else (the
/// user, another app) copied over it during the paste, and restoring would
/// race that copy and discard it.
pub(crate) fn inject_text(
    text: &str,
    is_terminal: bool,
    key_delay: std::time::Duration,
    backend: InjectorBackend,
    paste: bool,
) -> Result<()> {
    if !paste {
        return type_text_at_cursor(text, key_delay, backend);
    }

    let clipboard = xkb_type::default_clipboard();
    let saved = match clipboard.get_text() {
        Ok(s) => s,
        Err(e) => {
            debug!("clipboard unreadable as text, typing instead of pasting: {e:#}");
            return type_text_at_cursor(text, key_delay, backend);
        }
    };
    clipboard
        .set_text(text)
        .context("failed to set clipboard for paste injection")?;

    // Let the clipboard settle before the paste keystroke.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let paste_result = paste_via_keyboard(is_terminal, key_delay, backend);

    // Restore the user's clipboard after the paste has landed, regardless of
    // whether the keystroke succeeded — but only if it's safe to (see doc
    // comment above).
    let pasted_text = text.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let clipboard = xkb_type::default_clipboard();
        match clipboard.get_text() {
            Ok(current) if current == pasted_text => {
                if let Err(e) = clipboard.set_text(&saved) {
                    warn!("failed to restore clipboard: {e}");
                }
            }
            Ok(_) => {
                debug!("clipboard changed during paste injection; skipping restore");
            }
            Err(e) => {
                warn!("failed to read clipboard before restore, skipping restore: {e}");
            }
        }
    });

    paste_result
}

pub(crate) fn warm_keyboard(key_delay: std::time::Duration, backend: InjectorBackend) {
    let keyboard_slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
    let Ok(mut keyboard_guard) = keyboard_slot.lock() else {
        warn!("failed to initialize virtual keyboard: keyboard mutex poisoned");
        return;
    };

    if keyboard_guard.is_some() {
        return;
    }

    // Startup path: long settle delay so X11 has time to process
    // MappingNotify and attach the device keymap before the first key.
    match new_keyboard(key_delay, /* prewarm = */ true, backend) {
        Ok(kb) => {
            *keyboard_guard = Some(kb);
            info!("virtual keyboard initialized");
        }
        Err(e) => {
            warn!("failed to initialize virtual keyboard: {e:#}");
        }
    }
}

/// Build the uinput (evdev) keyboard, mapping the common permission failure
/// to an actionable message. `prewarm` adds a settle delay so X11 attaches
/// the device keymap before the first keystroke.
fn new_uinput_keyboard(
    key_delay: std::time::Duration,
    prewarm: bool,
) -> Result<Box<dyn xkb_type::KeyInjector>> {
    let result = if prewarm {
        xkb_type::Keyboard::new_prewarm(key_delay)
    } else {
        xkb_type::Keyboard::new(key_delay)
    };
    match result {
        Ok(kb) => Ok(Box::new(kb)),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("Permission denied") || msg.contains("permission") {
                anyhow::bail!(
                    "Cannot open /dev/uinput — permission denied.\n\
                     Fix: sudo usermod -aG input $USER"
                );
            }
            Err(e.context("failed to create virtual keyboard"))
        }
    }
}

/// Construct the configured keyboard-injection backend.
///
/// `Auto` prefers the layout-independent Wayland virtual keyboard when a
/// Wayland session is detected, falling back to uinput when the compositor
/// lacks `zwp_virtual_keyboard_v1`. `prewarm` only affects the uinput path
/// (the Wayland backend ships its own keymap, so no settle delay is needed).
fn new_keyboard(
    key_delay: std::time::Duration,
    prewarm: bool,
    backend: InjectorBackend,
) -> Result<Box<dyn xkb_type::KeyInjector>> {
    match backend {
        InjectorBackend::Uinput => new_uinput_keyboard(key_delay, prewarm),
        InjectorBackend::WaylandVk => {
            let kb = xkb_type::wayland_vk::WaylandVkKeyboard::new(key_delay)?;
            info!("using wayland virtual-keyboard injection backend");
            Ok(Box::new(kb))
        }
        InjectorBackend::Auto => {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                match xkb_type::wayland_vk::WaylandVkKeyboard::new(key_delay) {
                    Ok(kb) => {
                        info!("using wayland virtual-keyboard injection backend");
                        return Ok(Box::new(kb));
                    }
                    Err(e) => {
                        warn!(
                            "wayland virtual-keyboard unavailable, falling back to uinput: {e:#}"
                        );
                    }
                }
            }
            new_uinput_keyboard(key_delay, prewarm)
        }
    }
}

/// Known terminal emulator window classes (lowercase for matching).
const TERMINAL_CLASSES: &[&str] = &[
    "alacritty",
    "kitty",
    "foot",
    "wezterm",
    "gnome-terminal",
    "konsole",
    "xterm",
    "urxvt",
    "terminator",
    "tilix",
    "st",
    "xfce4-terminal",
    "sakura",
    "guake",
    "yakuake",
    "termite",
    "cool-retro-term",
    "ghostty",
];

/// Check if a window class corresponds to a terminal emulator.
pub(crate) fn is_terminal_class(class: &str) -> bool {
    let lower = class.to_lowercase();
    TERMINAL_CLASSES.iter().any(|t| lower.contains(t))
}
