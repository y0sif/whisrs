//! Linux global hotkey listener via evdev input devices.

use std::collections::HashSet;
use std::path::Path;

use evdev::{Device, EventType, InputEventKind, Key};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub use super::parse::{parse_hotkey, HotkeyBinding};
use crate::{Command, HotkeyConfig};

/// A configured hotkey action.
struct HotkeyAction {
    binding: HotkeyBinding,
    command: Command,
}

/// Start the global hotkey listener.
///
/// Enumerates keyboard input devices, listens for key events, and sends
/// matching commands through the provided channel. Runs until dropped.
pub async fn start_hotkey_listener(config: &HotkeyConfig, cmd_tx: mpsc::Sender<Command>) {
    let mut actions = Vec::new();

    if let Some(ref s) = config.toggle {
        match parse_hotkey(s) {
            Ok(binding) => {
                info!("hotkey: toggle = {s}");
                actions.push(HotkeyAction {
                    binding,
                    command: Command::Toggle,
                });
            }
            Err(e) => warn!("invalid toggle hotkey '{s}': {e}"),
        }
    }

    if let Some(ref s) = config.cancel {
        match parse_hotkey(s) {
            Ok(binding) => {
                info!("hotkey: cancel = {s}");
                actions.push(HotkeyAction {
                    binding,
                    command: Command::Cancel,
                });
            }
            Err(e) => warn!("invalid cancel hotkey '{s}': {e}"),
        }
    }

    if let Some(ref s) = config.command {
        match parse_hotkey(s) {
            Ok(binding) => {
                info!("hotkey: command = {s}");
                actions.push(HotkeyAction {
                    binding,
                    command: Command::CommandMode,
                });
            }
            Err(e) => warn!("invalid command hotkey '{s}': {e}"),
        }
    }

    if actions.is_empty() {
        debug!("no hotkeys configured");
        return;
    }

    // Find all keyboard input devices.
    let devices = match enumerate_keyboards() {
        Ok(d) if d.is_empty() => {
            warn!("no keyboard input devices found — hotkeys disabled");
            return;
        }
        Ok(d) => d,
        Err(e) => {
            warn!("failed to enumerate input devices: {e} — hotkeys disabled");
            return;
        }
    };

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

/// Check if all required modifier keys (or their left/right variants) are held.
fn modifiers_held(held: &HashSet<Key>, required: &[Key]) -> bool {
    required.iter().all(|m| {
        // Accept either left or right variant.
        match *m {
            Key::KEY_LEFTMETA => {
                held.contains(&Key::KEY_LEFTMETA) || held.contains(&Key::KEY_RIGHTMETA)
            }
            Key::KEY_LEFTALT => {
                held.contains(&Key::KEY_LEFTALT) || held.contains(&Key::KEY_RIGHTALT)
            }
            Key::KEY_LEFTCTRL => {
                held.contains(&Key::KEY_LEFTCTRL) || held.contains(&Key::KEY_RIGHTCTRL)
            }
            Key::KEY_LEFTSHIFT => {
                held.contains(&Key::KEY_LEFTSHIFT) || held.contains(&Key::KEY_RIGHTSHIFT)
            }
            other => held.contains(&other),
        }
    })
}
