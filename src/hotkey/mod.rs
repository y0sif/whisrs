//! Global hotkey listener via evdev input devices.
//!
//! Passively monitors keyboard input devices for configured key combos
//! and sends commands to the daemon when they match.

mod parse;

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use evdev::{Device, EventType, InputEventKind, Key};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::llm::LlmCommandConfig;
use crate::{Command, HotkeyConfig};
pub use parse::{parse_hotkey, HotkeyBinding};

/// Maximum number of attempts to find keyboard input devices.
const HOTKEY_MAX_RETRIES: u32 = 10;

/// Initial retry delay (doubles each attempt, capped at 10 s).
const HOTKEY_INITIAL_DELAY: Duration = Duration::from_secs(1);

/// A configured hotkey action.
struct HotkeyAction {
    binding: HotkeyBinding,
    command: Command,
}

/// Start the global hotkey listener.
///
/// Enumerates keyboard input devices, listens for key events, and sends
/// matching commands through the provided channel. Retries with exponential
/// backoff if no keyboards are found yet (common on boot when the daemon
/// starts before input devices are fully initialized). Runs until dropped.
pub async fn start_hotkey_listener(
    config: &HotkeyConfig,
    llm_commands: &[LlmCommandConfig],
    cmd_tx: mpsc::Sender<Command>,
) {
    let actions = build_actions(config, llm_commands);

    if actions.is_empty() {
        debug!("no hotkeys configured");
        return;
    }

    // Find keyboard input devices, retrying with backoff on boot.
    let mut delay = HOTKEY_INITIAL_DELAY;
    let mut devices = Vec::new();

    for attempt in 1..=HOTKEY_MAX_RETRIES {
        match enumerate_keyboards() {
            Ok(d) if !d.is_empty() => {
                if attempt > 1 {
                    info!("found {} keyboard device(s) (attempt {attempt})", d.len());
                }
                devices = d;
                break;
            }
            Ok(_) => {
                if attempt == HOTKEY_MAX_RETRIES {
                    warn!(
                        "no keyboard input devices found after {HOTKEY_MAX_RETRIES} attempts — hotkeys disabled"
                    );
                    return;
                }
                info!(
                    "no keyboard devices found (attempt {attempt}/{HOTKEY_MAX_RETRIES}) — retrying in {delay:?}"
                );
            }
            Err(e) => {
                if attempt == HOTKEY_MAX_RETRIES {
                    warn!(
                        "failed to enumerate input devices after {HOTKEY_MAX_RETRIES} attempts: {e} — hotkeys disabled"
                    );
                    return;
                }
                info!(
                    "failed to enumerate input devices (attempt {attempt}/{HOTKEY_MAX_RETRIES}): {e} — retrying in {delay:?}"
                );
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(10));
    }

    info!(
        "hotkey listener monitoring {} keyboard device(s)",
        devices.len()
    );

    // Spawn a listener task for each device.
    for device in devices {
        let name = device.name().unwrap_or("unknown").to_string();
        let actions_clone: Vec<(Vec<Key>, Key, Command)> = actions
            .iter()
            .map(|a| {
                (
                    a.binding.modifiers.clone(),
                    a.binding.trigger,
                    a.command.clone(),
                )
            })
            .collect();
        let tx = cmd_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = listen_device(device, &actions_clone, tx).await {
                debug!("hotkey listener for '{name}' stopped: {e}");
            }
        });
    }
}

/// Build the dispatch table: every set `[hotkeys]` field, plus every
/// `[[llm_commands]]` entry's `hotkey` and optional `set_hotkey`. Invalid
/// specs are warned about and skipped, so one bad combo never disables the
/// rest.
///
/// Split out of [`start_hotkey_listener`] — which needs real input devices and
/// so cannot be unit-tested — because the failure this guards against is
/// silent: a field added to [`HotkeyConfig`] but not listed here parses fine,
/// validates fine, shows up in the config editor, and simply never fires.
/// `every_fixed_hotkey_field_is_dispatched` below fails when that happens.
fn build_actions(config: &HotkeyConfig, llm_commands: &[LlmCommandConfig]) -> Vec<HotkeyAction> {
    let mut actions = Vec::new();

    // Every field of `HotkeyConfig` must appear in this table.
    let fixed = [
        ("toggle", &config.toggle, Command::Toggle { language: None }),
        ("cancel", &config.cancel, Command::Cancel),
        ("command", &config.command, Command::CommandMode),
        ("speak", &config.speak, Command::Speak),
    ];
    for (label, spec, command) in fixed {
        let Some(spec) = spec else { continue };
        match parse_hotkey(spec) {
            Ok(binding) => {
                info!("hotkey: {label} = {spec}");
                actions.push(HotkeyAction { binding, command });
            }
            Err(e) => warn!("invalid {label} hotkey '{spec}': {e}"),
        }
    }

    for entry in llm_commands {
        match parse_hotkey(&entry.hotkey) {
            Ok(binding) => {
                info!("hotkey: llm-command '{}' = {}", entry.name, entry.hotkey);
                actions.push(HotkeyAction {
                    binding,
                    command: Command::LlmCommand {
                        name: entry.name.clone(),
                    },
                });
            }
            Err(e) => warn!(
                "invalid hotkey '{}' for llm-command '{}': {e}",
                entry.hotkey, entry.name
            ),
        }

        if let Some(set_hotkey) = &entry.set_hotkey {
            match parse_hotkey(set_hotkey) {
                Ok(binding) => {
                    info!(
                        "hotkey: llm-command '{}' set-instruction = {}",
                        entry.name, set_hotkey
                    );
                    actions.push(HotkeyAction {
                        binding,
                        command: Command::SetLlmInstruction {
                            name: entry.name.clone(),
                        },
                    });
                }
                Err(e) => warn!(
                    "invalid set_hotkey '{}' for llm-command '{}': {e}",
                    set_hotkey, entry.name
                ),
            }
        }
    }

    actions
}

/// Enumerate all keyboard input devices.
fn enumerate_keyboards() -> anyhow::Result<Vec<Device>> {
    let mut keyboards = Vec::new();
    let input_dir = Path::new("/dev/input");

    if !input_dir.exists() {
        anyhow::bail!("/dev/input does not exist");
    }

    for entry in std::fs::read_dir(input_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Only look at eventN devices.
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("event") {
            continue;
        }

        match Device::open(&path) {
            Ok(device) => {
                // Check if this device has keyboard capabilities.
                if let Some(keys) = device.supported_keys() {
                    if keys.contains(Key::KEY_A) && keys.contains(Key::KEY_LEFTMETA) {
                        let dev_name = device.name().unwrap_or("unknown").to_string();
                        debug!("found keyboard: {} ({})", dev_name, path.display());
                        keyboards.push(device);
                    }
                }
            }
            Err(e) => {
                debug!("cannot open {}: {e}", path.display());
            }
        }
    }

    Ok(keyboards)
}

/// Listen on a single device for hotkey combos.
async fn listen_device(
    device: Device,
    actions: &[(Vec<Key>, Key, Command)],
    cmd_tx: mpsc::Sender<Command>,
) -> anyhow::Result<()> {
    // Track which keys are currently held.
    let mut held_keys: HashSet<Key> = HashSet::new();

    // Wrap device in async fd.
    let mut stream = device.into_event_stream()?;

    loop {
        let event = stream.next_event().await?;

        if event.event_type() != EventType::KEY {
            continue;
        }

        let key = match event.kind() {
            InputEventKind::Key(k) => k,
            _ => continue,
        };

        match event.value() {
            1 => {
                // Key press.
                held_keys.insert(key);

                // Check if any hotkey combo matches.
                for (modifiers, trigger, command) in actions {
                    if key == *trigger && modifiers_held(&held_keys, modifiers) {
                        debug!("hotkey matched: {:?}", command);
                        let _ = cmd_tx.send(command.clone()).await;
                    }
                }
            }
            0 => {
                // Key release.
                held_keys.remove(&key);
            }
            _ => {} // Repeat (2) — ignore.
        }
    }
}

/// True iff *exactly* the required modifiers are held — no more, no fewer —
/// with left/right variants treated as equivalent.
///
/// Requiring an exact set (not just a subset) means a less-specific binding
/// (e.g. `Ctrl+Alt+X`) does NOT also match when a more-specific one
/// (`Ctrl+Alt+Shift+X`) is pressed, so both can be bound to the same trigger
/// key with different modifier sets without shadowing each other.
fn modifiers_held(held: &HashSet<Key>, required: &[Key]) -> bool {
    /// Collapse right-hand modifier variants onto their left counterpart;
    /// non-modifier keys map to themselves.
    fn canon(k: Key) -> Key {
        match k {
            Key::KEY_RIGHTMETA => Key::KEY_LEFTMETA,
            Key::KEY_RIGHTALT => Key::KEY_LEFTALT,
            Key::KEY_RIGHTCTRL => Key::KEY_LEFTCTRL,
            Key::KEY_RIGHTSHIFT => Key::KEY_LEFTSHIFT,
            other => other,
        }
    }
    fn is_modifier(k: Key) -> bool {
        matches!(
            canon(k),
            Key::KEY_LEFTMETA | Key::KEY_LEFTALT | Key::KEY_LEFTCTRL | Key::KEY_LEFTSHIFT
        )
    }

    let required_canon: HashSet<Key> = required.iter().map(|k| canon(*k)).collect();

    // Every required modifier must be held...
    if !required_canon
        .iter()
        .all(|m| held.iter().any(|h| canon(*h) == *m))
    {
        return false;
    }
    // ...and no modifier outside the required set may be held.
    !held
        .iter()
        .any(|h| is_modifier(*h) && !required_canon.contains(&canon(*h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_modifier_match() {
        let ctrl_alt = [Key::KEY_LEFTCTRL, Key::KEY_LEFTALT];
        let ctrl_alt_shift = [Key::KEY_LEFTCTRL, Key::KEY_LEFTALT, Key::KEY_LEFTSHIFT];

        // Ctrl+Alt held → matches Ctrl+Alt, not Ctrl+Alt+Shift.
        let held: HashSet<Key> =
            HashSet::from([Key::KEY_LEFTCTRL, Key::KEY_LEFTALT, Key::KEY_PAGEUP]);
        assert!(modifiers_held(&held, &ctrl_alt));
        assert!(!modifiers_held(&held, &ctrl_alt_shift));

        // Ctrl+Alt+Shift held → matches Ctrl+Alt+Shift, NOT the Ctrl+Alt
        // subset (the extra Shift must disqualify it — the shadowing bug).
        let held: HashSet<Key> = HashSet::from([
            Key::KEY_LEFTCTRL,
            Key::KEY_LEFTALT,
            Key::KEY_LEFTSHIFT,
            Key::KEY_PAGEUP,
        ]);
        assert!(modifiers_held(&held, &ctrl_alt_shift));
        assert!(!modifiers_held(&held, &ctrl_alt));
    }

    #[test]
    fn right_hand_modifiers_are_equivalent() {
        let held: HashSet<Key> = HashSet::from([Key::KEY_RIGHTCTRL, Key::KEY_PAGEUP]);
        assert!(modifiers_held(&held, &[Key::KEY_LEFTCTRL]));
    }

    fn all_hotkeys_set() -> HotkeyConfig {
        HotkeyConfig {
            toggle: Some("Super+Shift+W".to_string()),
            cancel: Some("Super+Shift+D".to_string()),
            command: Some("Super+Shift+G".to_string()),
            speak: Some("Super+Shift+R".to_string()),
        }
    }

    /// Every field of [`HotkeyConfig`] must be wired to a command in
    /// [`build_actions`]. Derived from serde rather than a hand-written count,
    /// so a fifth field added to the struct fails here instead of shipping as
    /// a binding that parses, validates, and silently never fires.
    #[test]
    fn every_fixed_hotkey_field_is_dispatched() {
        let config = all_hotkeys_set();
        let fields = toml::Value::try_from(&config)
            .expect("HotkeyConfig serializes")
            .as_table()
            .expect("as a table")
            .len();

        let actions = build_actions(&config, &[]);
        assert_eq!(
            actions.len(),
            fields,
            "every set [hotkeys] field must produce a listener action — a new field \
             needs a line in build_actions' `fixed` table"
        );
    }

    /// Each field is wired to its own command — a mis-ordered `fixed` table
    /// would silently send one key's press through another's handler.
    #[test]
    fn each_field_dispatches_its_own_command() {
        let actions = build_actions(&all_hotkeys_set(), &[]);
        for expected in [
            Command::Toggle { language: None },
            Command::Cancel,
            Command::CommandMode,
            Command::Speak,
        ] {
            assert_eq!(
                actions
                    .iter()
                    .filter(
                        |a| std::mem::discriminant(&a.command) == std::mem::discriminant(&expected)
                    )
                    .count(),
                1,
                "{expected:?} must be dispatched exactly once"
            );
        }
    }

    /// An unset binding registers nothing.
    #[test]
    fn an_unset_binding_registers_nothing() {
        let config = HotkeyConfig {
            command: Some("Super+Shift+G".to_string()),
            ..Default::default()
        };
        let actions = build_actions(&config, &[]);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0].command, Command::CommandMode));
    }

    /// One invalid spec must not take the others down with it.
    #[test]
    fn an_invalid_binding_is_skipped_not_fatal() {
        let config = HotkeyConfig {
            toggle: Some("Super+Shift+W".to_string()),
            speak: Some("NotAKey".to_string()),
            ..Default::default()
        };
        let actions = build_actions(&config, &[]);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0].command, Command::Toggle { .. }));
    }
}
