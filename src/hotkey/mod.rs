//! Global hotkey listener via evdev input devices.
//!
//! Passively monitors keyboard input devices for configured key combos
//! and sends commands to the daemon when they match.

mod parse;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use evdev::{AttributeSetRef, Device, EventType, InputEventKind, Key};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::agent::{Agent, AgentAction};
use crate::llm::LlmCommandConfig;
#[cfg(feature = "recording")]
use crate::recording::{RecordingAction, RecordingOptions};
use crate::{Command, HotkeyConfig};
pub use parse::{hotkey_parts, parse_hotkey, HotkeyBinding, KEY_NAMES, MODIFIER_NAMES};

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
    agents: &[Agent],
    cmd_tx: mpsc::Sender<Command>,
) {
    let actions = build_actions(config, llm_commands, agents);

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

/// Build the dispatch table: every set `[hotkeys]` field, every
/// `[[llm_commands]]` entry's `hotkey` and optional `set_hotkey`, and every
/// usable `[[agents]]` entry's `hotkey` (a voice turn) and
/// `new_session_hotkey` (one in a fresh session). Invalid specs are
/// warned about and skipped, so one bad combo never disables the rest.
///
/// Split out of [`start_hotkey_listener`] — which needs real input devices and
/// so cannot be unit-tested — because the failure this guards against is
/// silent: a field added to [`HotkeyConfig`] but not listed here parses fine,
/// validates fine, shows up in the config editor, and simply never fires.
/// `every_fixed_hotkey_field_is_dispatched` below fails when that happens.
fn build_actions(
    config: &HotkeyConfig,
    llm_commands: &[LlmCommandConfig],
    agents: &[Agent],
) -> Vec<HotkeyAction> {
    let mut actions = Vec::new();

    // Every field of `HotkeyConfig` must appear in this table.
    let fixed = [
        ("toggle", &config.toggle, Command::Toggle { language: None }),
        ("cancel", &config.cancel, Command::Cancel),
        ("command", &config.command, Command::CommandMode),
        ("speak", &config.speak, Command::Speak),
        // A build without the recording feature binds nothing for it; the
        // config warns that `[hotkeys] recording` is ignored.
        #[cfg(feature = "recording")]
        (
            "recording",
            &config.recording,
            Command::Recording {
                action: RecordingAction::Toggle,
                title: None,
                id: None,
                options: RecordingOptions::default(),
            },
        ),
        (
            "next_profile",
            &config.next_profile,
            Command::SwitchProfile { name: None },
        ),
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

    for agent in agents {
        for (key, spec, new) in [
            ("hotkey", &agent.hotkey, false),
            ("new_session_hotkey", &agent.new_session_hotkey, true),
        ] {
            let Some(spec) = spec else { continue };
            match parse_hotkey(spec) {
                Ok(binding) => {
                    info!("hotkey: agent '{}' {key} = {spec}", agent.name);
                    actions.push(HotkeyAction {
                        binding,
                        command: Command::Agent {
                            action: AgentAction::Toggle,
                            name: Some(agent.name.clone()),
                            text: None,
                            new,
                        },
                    });
                }
                Err(e) => warn!("invalid {key} '{spec}' for agent '{}': {e}", agent.name),
            }
        }
    }

    actions
}

/// Modifier keys, left and right. A physically held one turns injected
/// keystrokes into shortcuts (#154).
const MODIFIER_KEYS: [Key; 8] = [
    Key::KEY_LEFTMETA,
    Key::KEY_RIGHTMETA,
    Key::KEY_LEFTALT,
    Key::KEY_RIGHTALT,
    Key::KEY_LEFTCTRL,
    Key::KEY_RIGHTCTRL,
    Key::KEY_LEFTSHIFT,
    Key::KEY_RIGHTSHIFT,
];

/// Whether `keys` (a device's `EVIOCGKEY` state) holds any modifier down.
///
/// `pub` only so the `whisrsd` binary crate can reach it. Hidden from the
/// docs to keep it off the supported surface.
#[doc(hidden)]
pub fn any_modifier_down(keys: &AttributeSetRef<Key>) -> bool {
    MODIFIER_KEYS.iter().any(|&k| keys.contains(k))
}

/// Enumerate all keyboard input devices.
///
/// Opens each device without grabbing it. Used by the hotkey listener; the
/// injection path uses the incremental [`KeyboardCache`] instead.
fn enumerate_keyboards() -> anyhow::Result<Vec<Device>> {
    let input_dir = Path::new("/dev/input");

    if !input_dir.exists() {
        anyhow::bail!("/dev/input does not exist");
    }

    let mut keyboards = Vec::new();
    for entry in std::fs::read_dir(input_dir)? {
        let path = entry?.path();
        if !is_event_node(&path) {
            continue;
        }
        if let Some(device) = open_keyboard(&path) {
            keyboards.push(device);
        }
    }

    Ok(keyboards)
}

/// Whether `path` names an `eventN` node.
fn is_event_node(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("event"))
}

/// Open `path` (without grabbing it) and return it if it is a keyboard.
/// `None` when it is not a keyboard or cannot be opened.
fn open_keyboard(path: &Path) -> Option<Device> {
    let device = match Device::open(path) {
        Ok(device) => device,
        Err(e) => {
            debug!("cannot open {}: {e}", path.display());
            return None;
        }
    };
    is_keyboard(&device, path).then_some(device)
}

/// Whether `device` is a keyboard: it reports both `KEY_A` and `KEY_LEFTMETA`.
fn is_keyboard(device: &Device, path: &Path) -> bool {
    let Some(keys) = device.supported_keys() else {
        return false;
    };
    if !(keys.contains(Key::KEY_A) && keys.contains(Key::KEY_LEFTMETA)) {
        return false;
    }
    let dev_name = device.name().unwrap_or("unknown");
    debug!("found keyboard: {} ({})", dev_name, path.display());
    true
}

/// Identity of a `/dev/input` node: its path plus its inode. The inode
/// changes when a node is recreated, so a replugged device that reuses an
/// `eventN` number is reclassified rather than served from the cache.
type NodeId = (PathBuf, u64);

/// Keyboards open for read-only key-state probes (#154), cached per node so
/// a probe does not reopen every `/dev/input/event*` node each time (that
/// costs 100+ ms; the key-state read itself is microseconds).
///
/// [`refresh`](Self::refresh) lists `/dev/input`, opens only new nodes, and
/// drops vanished ones. A node that fails to open is not cached, so it is
/// retried next time: udev applies the `input` group a few ms after a
/// hotplugged node appears, and a probe in that window must not skip the
/// keyboard for good. [`any_modifier_held`](Self::any_modifier_held)
/// evicts a device whose read fails (unplugged), so it is reclassified if
/// its node comes back. Devices are never grabbed.
///
/// `pub` only so the `whisrsd` binary crate can reach it. Hidden from the
/// docs to keep it off the supported surface.
#[doc(hidden)]
pub struct KeyboardCache {
    /// Device names never probed: our own uinput devices, whose modifiers
    /// are the ones we press.
    skip_names: &'static [&'static str],
    /// `Some` = a keyboard to probe; `None` = not a keyboard, or skipped by
    /// name. Unopenable nodes are left out so they are retried.
    nodes: HashMap<NodeId, Option<Device>>,
}

impl KeyboardCache {
    /// An empty cache that will ignore devices named in `skip_names`.
    pub fn new(skip_names: &'static [&'static str]) -> Self {
        Self {
            skip_names,
            nodes: HashMap::new(),
        }
    }

    /// Bring the cache in line with `/dev/input`: classify new nodes, drop
    /// vanished ones. Cheap when nothing changed (one `read_dir`).
    pub fn refresh(&mut self) -> anyhow::Result<()> {
        use std::os::unix::fs::DirEntryExt;

        let mut current = HashSet::new();
        for entry in std::fs::read_dir("/dev/input")? {
            let entry = entry?;
            let path = entry.path();
            if is_event_node(&path) {
                current.insert((path, entry.ino()));
            }
        }

        let (added, removed) = diff_nodes(self.nodes.keys(), &current);
        for node in removed {
            self.nodes.remove(&node);
        }
        for node in added {
            let device = match Device::open(&node.0) {
                Ok(device) => device,
                Err(e) => {
                    debug!("cannot open {}: {e}", node.0.display());
                    continue;
                }
            };
            let probe = is_keyboard(&device, &node.0)
                && !device.name().is_some_and(|n| self.skip_names.contains(&n));
            self.nodes.insert(node, probe.then_some(device));
        }
        Ok(())
    }

    /// Whether any cached keyboard reports a modifier down (`EVIOCGKEY`).
    /// A device whose read fails is evicted and counts as not held.
    pub fn any_modifier_held(&mut self) -> bool {
        let mut held = false;
        let mut failed = Vec::new();
        for (node, device) in &self.nodes {
            let Some(device) = device else { continue };
            match device.get_key_state() {
                Ok(keys) => {
                    if any_modifier_down(&keys) {
                        held = true;
                        break;
                    }
                }
                Err(e) => {
                    debug!("cannot read key state of {}: {e}", node.0.display());
                    failed.push(node.clone());
                }
            }
        }
        for node in failed {
            self.nodes.remove(&node);
        }
        held
    }

    /// Number of cached keyboards (probed devices).
    pub fn keyboard_count(&self) -> usize {
        self.nodes.values().filter(|d| d.is_some()).count()
    }
}

/// Split the node sets: (in `current` but not `cached`, in `cached` but not
/// `current`). Pure, so the cache bookkeeping is testable without
/// `/dev/input`.
fn diff_nodes<'a>(
    cached: impl Iterator<Item = &'a NodeId>,
    current: &HashSet<NodeId>,
) -> (Vec<NodeId>, Vec<NodeId>) {
    let cached: HashSet<&NodeId> = cached.collect();
    let added = current
        .iter()
        .filter(|n| !cached.contains(n))
        .cloned()
        .collect();
    let removed = cached
        .into_iter()
        .filter(|n| !current.contains(*n))
        .cloned()
        .collect();
    (added, removed)
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

    #[test]
    fn bare_binding_requires_no_modifier_held() {
        // A bare binding ("ScrollLock") fires only when no modifier is held,
        // so it stays distinct from "Shift+ScrollLock".
        let held: HashSet<Key> = HashSet::from([Key::KEY_SCROLLLOCK]);
        assert!(modifiers_held(&held, &[]));

        for m in [Key::KEY_LEFTSHIFT, Key::KEY_RIGHTCTRL, Key::KEY_LEFTMETA] {
            let held: HashSet<Key> = HashSet::from([m, Key::KEY_SCROLLLOCK]);
            assert!(!modifiers_held(&held, &[]), "{m:?} held must not match");
        }
    }

    fn all_hotkeys_set() -> HotkeyConfig {
        HotkeyConfig {
            toggle: Some("Super+Shift+W".to_string()),
            cancel: Some("Super+Shift+D".to_string()),
            command: Some("Super+Shift+G".to_string()),
            speak: Some("Super+Shift+R".to_string()),
            recording: Some("Super+Shift+M".to_string()),
            next_profile: Some("Super+Shift+P".to_string()),
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

        let actions = build_actions(&config, &[], &[]);
        // `[hotkeys] recording` alone binds nothing in a build without the
        // recording feature, which warns that it is ignored.
        let ignored = usize::from(!cfg!(feature = "recording"));
        assert_eq!(
            actions.len(),
            fields - ignored,
            "every set [hotkeys] field must produce a listener action — a new field \
             needs a line in build_actions' `fixed` table"
        );
    }

    /// `[hotkeys] next_profile` sends the switch with no name: the daemon
    /// picks the next profile.
    #[test]
    fn next_profile_hotkey_switches_to_the_next_profile() {
        let config = HotkeyConfig {
            next_profile: Some("Super+Shift+P".to_string()),
            ..Default::default()
        };
        let actions = build_actions(&config, &[], &[]);
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(actions[0].command, Command::SwitchProfile { name: None }),
            "{:?}",
            actions[0].command
        );
    }

    /// Built without the recording feature, `[hotkeys] recording` registers
    /// no key: pressing it would only reach a daemon that refuses it.
    #[cfg(not(feature = "recording"))]
    #[test]
    fn recording_hotkey_binds_nothing_without_the_feature() {
        let config = HotkeyConfig {
            recording: Some("ScrollLock".to_string()),
            ..Default::default()
        };
        assert!(build_actions(&config, &[], &[]).is_empty());
        let actions = build_actions(&all_hotkeys_set(), &[], &[]);
        assert!(actions
            .iter()
            .all(|a| !matches!(a.command, Command::Recording { .. })));
    }

    /// `[hotkeys] recording` takes the same forms as the other fields, the
    /// bare ScrollLock/Pause/F13-F24 bindings included.
    #[cfg(feature = "recording")]
    #[test]
    fn recording_hotkey_accepts_a_bare_binding() {
        let config = HotkeyConfig {
            recording: Some("ScrollLock".to_string()),
            ..Default::default()
        };
        let actions = build_actions(&config, &[], &[]);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].binding.modifiers.is_empty());
        assert_eq!(actions[0].binding.trigger, Key::KEY_SCROLLLOCK);
        assert!(matches!(
            actions[0].command,
            Command::Recording {
                action: RecordingAction::Toggle,
                ..
            }
        ));
    }

    /// Each field is wired to its own command — a mis-ordered `fixed` table
    /// would silently send one key's press through another's handler.
    #[test]
    fn each_field_dispatches_its_own_command() {
        let actions = build_actions(&all_hotkeys_set(), &[], &[]);
        for expected in [
            Command::Toggle { language: None },
            Command::Cancel,
            Command::CommandMode,
            Command::Speak,
            #[cfg(feature = "recording")]
            Command::Recording {
                action: RecordingAction::Toggle,
                title: None,
                id: None,
                options: RecordingOptions::default(),
            },
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
        let actions = build_actions(&config, &[], &[]);
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
        let actions = build_actions(&config, &[], &[]);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0].command, Command::Toggle { .. }));
    }

    /// An agent's hotkey records a voice turn for that agent, and its
    /// `new_session_hotkey` one in a fresh session; one without a hotkey
    /// binds nothing.
    #[test]
    fn agent_hotkeys_dispatch_a_voice_turn() {
        use crate::agent::{Harness, Permissions};
        let agent = |name: &str, hotkey: Option<&str>| Agent {
            name: name.to_string(),
            hotkey: hotkey.map(str::to_string),
            new_session_hotkey: None,
            harness: Harness::Claude,
            dir: PathBuf::from("/proj"),
            permissions: Permissions::Auto,
            command: "claude".to_string(),
            args: Vec::new(),
            instruction: String::new(),
            feedback: crate::agent::progress::Feedback::Brief,
        };
        let mut jarvis = agent("jarvis", Some("Super+Shift+J"));
        jarvis.new_session_hotkey = Some("Super+Shift+N".to_string());
        let mut broken = agent("broken", Some("Super+Nope"));
        broken.new_session_hotkey = Some("Super+Nada".to_string());
        let mut only_new = agent("fresh", None);
        only_new.new_session_hotkey = Some("Super+Shift+F".to_string());
        let agents = [jarvis, agent("quiet", None), broken, only_new];
        let actions = build_actions(&HotkeyConfig::default(), &[], &agents);
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].binding.trigger, Key::KEY_J);
        assert!(matches!(
            &actions[0].command,
            Command::Agent { action: AgentAction::Toggle, name: Some(n), text: None, new: false }
                if n == "jarvis"
        ));
        assert_eq!(actions[1].binding.trigger, Key::KEY_N);
        assert!(matches!(
            &actions[1].command,
            Command::Agent { action: AgentAction::Toggle, name: Some(n), text: None, new: true }
                if n == "jarvis"
        ));
        assert_eq!(actions[2].binding.trigger, Key::KEY_F);
        assert!(matches!(
            &actions[2].command,
            Command::Agent { action: AgentAction::Toggle, name: Some(n), new: true, .. }
                if n == "fresh"
        ));
    }

    fn key_set(keys: &[Key]) -> evdev::AttributeSet<Key> {
        let mut set = evdev::AttributeSet::new();
        for &k in keys {
            set.insert(k);
        }
        set
    }

    /// Every modifier counts, on either side of the keyboard.
    #[test]
    fn any_modifier_down_sees_left_and_right_modifiers() {
        for &k in &MODIFIER_KEYS {
            assert!(any_modifier_down(&key_set(&[k])), "{k:?} is a modifier");
        }
        assert!(any_modifier_down(&key_set(&[
            Key::KEY_W,
            Key::KEY_RIGHTALT
        ])));
    }

    /// Ordinary keys, and no keys at all, are not a held modifier.
    #[test]
    fn any_modifier_down_ignores_non_modifiers_and_empty() {
        assert!(!any_modifier_down(&key_set(&[])));
        assert!(!any_modifier_down(&key_set(&[
            Key::KEY_W,
            Key::KEY_A,
            Key::KEY_CAPSLOCK,
            Key::KEY_SPACE,
        ])));
    }

    fn node(path: &str, ino: u64) -> NodeId {
        (PathBuf::from(path), ino)
    }

    fn sorted(mut v: Vec<NodeId>) -> Vec<NodeId> {
        v.sort();
        v
    }

    /// Unchanged nodes need no work: nothing reopened, nothing dropped.
    #[test]
    fn diff_nodes_is_empty_when_nothing_changed() {
        let cached = [node("/dev/input/event0", 1), node("/dev/input/event1", 2)];
        let current: HashSet<NodeId> = cached.iter().cloned().collect();
        let (added, removed) = diff_nodes(cached.iter(), &current);
        assert!(added.is_empty() && removed.is_empty());
    }

    /// A plugged node is classified, an unplugged one dropped.
    #[test]
    fn diff_nodes_sees_hotplug_both_ways() {
        let cached = [node("/dev/input/event0", 1), node("/dev/input/event1", 2)];
        let current: HashSet<NodeId> = [node("/dev/input/event0", 1), node("/dev/input/event5", 9)]
            .into_iter()
            .collect();
        let (added, removed) = diff_nodes(cached.iter(), &current);
        assert_eq!(added, vec![node("/dev/input/event5", 9)]);
        assert_eq!(removed, vec![node("/dev/input/event1", 2)]);
    }

    /// A node number reused by a new device (new inode) is reclassified.
    #[test]
    fn diff_nodes_reclassifies_a_reused_node_number() {
        let cached = [node("/dev/input/event3", 7)];
        let current: HashSet<NodeId> = [node("/dev/input/event3", 8)].into_iter().collect();
        let (added, removed) = diff_nodes(cached.iter(), &current);
        assert_eq!(sorted(added), vec![node("/dev/input/event3", 8)]);
        assert_eq!(sorted(removed), vec![node("/dev/input/event3", 7)]);
    }

    #[test]
    fn is_event_node_only_matches_event_nodes() {
        assert!(is_event_node(Path::new("/dev/input/event12")));
        assert!(!is_event_node(Path::new("/dev/input/mice")));
        assert!(!is_event_node(Path::new("/dev/input/by-id")));
    }
}
