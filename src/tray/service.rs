//! System tray implementation using ksni (StatusNotifierItem).

use ksni::menu::{RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Icon, MenuItem, ToolTip, TrayMethods};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use super::{NotifyFn, ProfileMenu, RestartFn};
#[cfg(feature = "recording")]
use crate::recording::{RecordingAction, RecordingOptions, RecordingPhase, RecordingStatus};
use crate::{Command, State};
use icons::IconSet;

/// Speech-bubble tray icons, one set per dictation state plus two for the
/// recording feature, at several sizes so HiDPI panels get a sharp render instead of an
/// upscaled 16x16. Dictation states fill the bubble; a recording outlines it in
/// the matching color (red while capturing, amber while transcribing), so the
/// two read apart at a glance. A build without the `recording` feature has
/// neither recording set.
///
/// The PNGs in `src/tray/icons/` are rendered from the SVGs next to them:
/// `rsvg-convert -w N -h N <name>.svg -o <name>-N.png` for each size.
mod icons {
    use std::sync::OnceLock;

    use ksni::Icon;

    use crate::State;

    macro_rules! set_pngs {
        ($name:literal) => {
            [
                include_bytes!(concat!("icons/", $name, "-16.png")).as_slice(),
                include_bytes!(concat!("icons/", $name, "-22.png")).as_slice(),
                include_bytes!(concat!("icons/", $name, "-32.png")).as_slice(),
                include_bytes!(concat!("icons/", $name, "-48.png")).as_slice(),
            ]
        };
    }

    /// One icon design, in all its sizes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum IconSet {
        Idle,
        Recording,
        Transcribing,
        Synthesizing,
        Speaking,
        /// A recording (mic + system audio) is in progress while dictation is idle.
        #[cfg(feature = "recording")]
        RecordingActive,
        /// A recording is being transcribed while dictation is idle.
        #[cfg(feature = "recording")]
        RecordingProcessing,
    }

    impl IconSet {
        /// How many sets this build has.
        pub const COUNT: usize = if cfg!(feature = "recording") { 7 } else { 5 };

        /// Every set, in declaration order (the cache index is `set as usize`).
        pub const ALL: [IconSet; Self::COUNT] = [
            IconSet::Idle,
            IconSet::Recording,
            IconSet::Transcribing,
            IconSet::Synthesizing,
            IconSet::Speaking,
            #[cfg(feature = "recording")]
            IconSet::RecordingActive,
            #[cfg(feature = "recording")]
            IconSet::RecordingProcessing,
        ];

        /// The set for a dictation state.
        pub fn for_state(state: State) -> Self {
            match state {
                State::Idle => IconSet::Idle,
                State::Recording => IconSet::Recording,
                State::Transcribing => IconSet::Transcribing,
                State::Synthesizing => IconSet::Synthesizing,
                State::Speaking => IconSet::Speaking,
            }
        }

        fn pngs(self) -> [&'static [u8]; 4] {
            match self {
                IconSet::Idle => set_pngs!("idle"),
                IconSet::Recording => set_pngs!("recording"),
                IconSet::Transcribing => set_pngs!("transcribing"),
                IconSet::Synthesizing => set_pngs!("synthesizing"),
                IconSet::Speaking => set_pngs!("speaking"),
                #[cfg(feature = "recording")]
                IconSet::RecordingActive => set_pngs!("recording-active"),
                #[cfg(feature = "recording")]
                IconSet::RecordingProcessing => set_pngs!("recording-processing"),
            }
        }
    }

    /// Decode an RGBA PNG into the ARGB (big-endian) pixmap SNI expects.
    fn decode(png_bytes: &[u8]) -> Icon {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder
            .read_info()
            .expect("embedded tray icon is a valid PNG");
        let mut buf = vec![
            0;
            reader
                .output_buffer_size()
                .expect("tray icon size fits in memory")
        ];
        let info = reader
            .next_frame(&mut buf)
            .expect("embedded tray icon decodes");
        assert_eq!(
            (info.color_type, info.bit_depth),
            (png::ColorType::Rgba, png::BitDepth::Eight),
            "tray icons must be 8-bit RGBA (rsvg-convert's output)"
        );
        let data = buf[..info.buffer_size()]
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[r, g, b, a]| [a, r, g, b])
            .collect();
        Icon {
            width: info.width as i32,
            height: info.height as i32,
            data,
        }
    }

    /// All sizes of `set`, decoded once and cached for the daemon's lifetime.
    pub fn get(set: IconSet) -> Vec<Icon> {
        static CACHE: OnceLock<[Vec<Icon>; IconSet::COUNT]> = OnceLock::new();
        let cache = CACHE
            .get_or_init(|| IconSet::ALL.map(|s| s.pngs().iter().map(|b| decode(b)).collect()));
        cache[set as usize].clone()
    }
}

/// Small mutable state owned by the tray service itself.
///
/// Keeping this directly on the tray object is important: `ksni::Handle::update`
/// expects the closure to mutate the tray instance so the host knows which
/// properties changed. When the state lives out-of-band, some tray hosts can
/// miss icon refreshes and leave the old color visible.
struct TrayState {
    current: State,
    #[cfg(feature = "recording")]
    recording: RecordingStatus,
}

/// One-line description of the recording, or `None` when no recording is active.
#[cfg(feature = "recording")]
fn recording_line(recording: &RecordingStatus) -> Option<String> {
    match recording.phase {
        RecordingPhase::Idle => None,
        RecordingPhase::Recording => {
            let mut line = "Recording mic and system audio".to_string();
            if let Some(started) = recording.started_at {
                line.push_str(&format!(" since {}", started.format("%H:%M")));
            }
            if let Some(title) = recording.title.as_deref().filter(|t| !t.is_empty()) {
                line.push_str(&format!(" ({title})"));
            }
            Some(line)
        }
        RecordingPhase::Processing => Some("Processing recording...".to_string()),
    }
}

/// Human-readable word for a state, used in the title and menu status line.
fn state_word(state: State) -> &'static str {
    match state {
        State::Idle => "idle",
        State::Recording => "dictating",
        State::Transcribing => "transcribing",
        State::Synthesizing => "synthesizing",
        State::Speaking => "speaking",
    }
}

/// The ksni tray implementation.
struct WhisrsTray {
    state: TrayState,
    /// Sender into the daemon's shared command dispatch loop — the same loop
    /// the hotkey listener feeds — so tray clicks drive `handle_command`
    /// exactly like hotkey presses do.
    cmd_tx: mpsc::Sender<Command>,
    /// Desktop-toast hook from the daemon (`None` when notifications are
    /// disabled), used to surface menu-callback failures the journal alone
    /// would hide — currently a failed "Restart Daemon" click.
    notify: Option<NotifyFn>,
    /// The "Profile" submenu, read at start.
    profiles: ProfileMenu,
    /// The daemon's restart, for "Restart Daemon".
    restart: RestartFn,
}

impl WhisrsTray {
    /// Queue a command for the daemon without blocking.
    ///
    /// ksni invokes tray callbacks on the tray service task and warns against
    /// blocking there, so use `try_send`; if the queue is somehow full the
    /// click is dropped with a warning instead of freezing the tray.
    fn send(&self, cmd: Command) {
        if let Err(e) = self.cmd_tx.try_send(cmd) {
            warn!("tray: failed to queue command for daemon: {e}");
        }
    }
}

impl ksni::Tray for WhisrsTray {
    fn id(&self) -> String {
        "whisrs".to_string()
    }

    fn title(&self) -> String {
        // A recording only takes over the title while dictation is idle: a
        // dictation started before the recording, or while it is transcribed,
        // matters more in the moment.
        #[cfg(feature = "recording")]
        match (self.state.current, self.state.recording.phase) {
            (State::Idle, RecordingPhase::Recording) => {
                return "whisrs: recording mic and system audio".to_string();
            }
            (State::Idle, RecordingPhase::Processing) => {
                return "whisrs: processing recording".to_string();
            }
            _ => {}
        }
        format!("whisrs: {}", state_word(self.state.current))
    }

    /// Left-click on the icon: toggle recording, same as `whisrs toggle`.
    fn activate(&mut self, _x: i32, _y: i32) {
        debug!("tray activated (left-click): toggle");
        self.send(Command::Toggle { language: None });
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        // A recording only takes over the icon while dictation is idle: a
        // dictation started before the recording, or while it is transcribed,
        // matters more in the moment.
        #[cfg(feature = "recording")]
        let set = match (self.state.current, self.state.recording.phase) {
            (State::Idle, RecordingPhase::Recording) => IconSet::RecordingActive,
            (State::Idle, RecordingPhase::Processing) => IconSet::RecordingProcessing,
            (state, _) => IconSet::for_state(state),
        };
        #[cfg(not(feature = "recording"))]
        let set = IconSet::for_state(self.state.current);
        icons::get(set)
    }

    fn tool_tip(&self) -> ToolTip {
        let description = match self.state.current {
            State::Idle => "Idle, ready to dictate",
            State::Recording => "Dictating...",
            State::Transcribing => "Transcribing...",
            State::Synthesizing => "Synthesizing…",
            State::Speaking => "Reading aloud…",
        };
        // The recording line leads while dictation is idle, and is appended
        // otherwise so a running recording is never hidden.
        #[cfg(feature = "recording")]
        let description = match (self.state.current, recording_line(&self.state.recording)) {
            (_, None) => description.to_string(),
            (State::Idle, Some(recording)) => recording,
            (_, Some(recording)) => format!("{description}\n{recording}"),
        };
        #[cfg(not(feature = "recording"))]
        let description = description.to_string();
        let description = match &self.profiles.active {
            Some(profile) => format!("{description}\nprofile: {profile}"),
            None => description,
        };
        ToolTip {
            title: "whisrs".to_string(),
            description,
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    /// Right-click menu.
    ///
    /// Deliberately holds no dictation controls: whisrs is driven by hotkeys,
    /// and toggle/cancel each already have a hotkey, a CLI command, and (for
    /// toggle) the left-click above. A menu item for them would be a fourth
    /// way to do the same thing. Recordings are the exception: they run for an
    /// hour, often started without a hotkey bound, and the tray is where the
    /// user sees one is running, so the start/stop/cancel controls live here.
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = self.state.current;
        let mut items = vec![
            // Non-interactive status line: version + current state.
            MenuItem::Standard(StandardItem {
                label: format!(
                    "whisrs v{} — {}",
                    env!("CARGO_PKG_VERSION"),
                    state_word(state)
                ),
                enabled: false,
                ..Default::default()
            }),
            MenuItem::Separator,
        ];
        #[cfg(feature = "recording")]
        {
            items.extend(recording_menu_items(&self.state.recording));
            items.push(MenuItem::Separator);
        }
        if let Some(menu) = profile_menu(&self.profiles) {
            items.extend([menu, MenuItem::Separator]);
        }
        items.extend([
            MenuItem::Standard(StandardItem {
                label: "Restart Daemon".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    restart_daemon(&tray.restart, tray.notify);
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|_tray: &mut Self| {
                    quit_daemon();
                }),
                ..Default::default()
            }),
        ]);
        items
    }
}

/// The recording section of the menu: a status line while a recording is active,
/// a start/stop toggle, and cancel while capturing.
#[cfg(feature = "recording")]
fn recording_menu_items(recording: &RecordingStatus) -> Vec<MenuItem<WhisrsTray>> {
    let mut items = Vec::new();
    if let Some(line) = recording_line(recording) {
        items.push(MenuItem::Standard(StandardItem {
            label: line,
            enabled: false,
            ..Default::default()
        }));
    }
    let capturing = recording.phase == RecordingPhase::Recording;
    items.push(MenuItem::Standard(StandardItem {
        label: if capturing {
            "Stop recording and transcribe"
        } else {
            "Start recording (mic + system audio)"
        }
        .to_string(),
        activate: Box::new(|tray: &mut WhisrsTray| {
            tray.send(recording_command(RecordingAction::Toggle));
        }),
        ..Default::default()
    }));
    if capturing {
        items.push(MenuItem::Standard(StandardItem {
            label: "Cancel recording and delete it".to_string(),
            activate: Box::new(|tray: &mut WhisrsTray| {
                tray.send(recording_command(RecordingAction::Cancel));
            }),
            ..Default::default()
        }));
    }
    items
}

/// The "Profile" submenu: base and every profile, the daemon's one
/// selected; picking one sends the switch. `None` without profiles.
fn profile_menu(profiles: &ProfileMenu) -> Option<MenuItem<WhisrsTray>> {
    if profiles.names.is_empty() {
        return None;
    }
    let options: Vec<String> = std::iter::once(crate::config::profile::BASE.to_string())
        .chain(profiles.names.iter().cloned())
        .collect();
    let selected = profiles
        .active
        .as_ref()
        .and_then(|active| options.iter().position(|o| o == active))
        .unwrap_or(0);
    let items = options
        .iter()
        .map(|name| RadioItem {
            // A lone `_` marks an access key in a menu label.
            label: name.replace('_', "__"),
            ..Default::default()
        })
        .collect();
    Some(
        SubMenu {
            label: "Profile".to_string(),
            submenu: vec![RadioGroup {
                selected,
                select: Box::new(move |tray: &mut WhisrsTray, index: usize| {
                    if let Some(name) = options.get(index) {
                        tray.send(Command::SwitchProfile {
                            name: Some(name.clone()),
                        });
                    }
                }),
                options: items,
            }
            .into()],
            ..Default::default()
        }
        .into(),
    )
}

#[cfg(feature = "recording")]
fn recording_command(action: RecordingAction) -> Command {
    Command::Recording {
        action,
        title: None,
        id: None,
        options: RecordingOptions::default(),
    }
}

/// Restart the daemon from the tray menu, through the daemon's own restart
/// ([`crate::service::restart_self`]: its service when it runs under one,
/// else a re-exec after the shutdown SIGTERM runs).
///
/// This cannot go through the daemon's own command loop: a successful restart
/// kills this very process before it could reply. `restart_self` runs on its
/// own thread because ksni menu callbacks must not block.
///
/// Failure raises a desktop toast (when `notify` is set), since log warnings
/// are invisible from the tray.
fn restart_daemon(restart: &RestartFn, notify: Option<NotifyFn>) {
    info!("tray: restart requested");
    restart(Box::new(move |why| {
        warn!("tray: restart failed: {why}");
        if let Some(notify) = notify {
            notify("whisrs", &format!("Daemon restart failed: {why}"));
        }
    }));
}

/// Quit from the tray menu by sending the daemon SIGTERM, so it takes the
/// same shutdown path as `systemctl stop`: a recording in progress is finalized
/// and the IPC socket removed before the process exits.
fn quit_daemon() {
    info!("tray: quit requested, shutting down");
    // SAFETY: kill(2) on our own pid has no memory-safety preconditions.
    let rc = unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
    if rc != 0 {
        // No handler could run; fall back to the old immediate exit.
        let _ = std::fs::remove_file(crate::socket_path());
        std::process::exit(0);
    }
}

/// Maximum number of attempts to connect to the SNI tray host.
const TRAY_MAX_RETRIES: u32 = 10;

/// Initial retry delay (doubles each attempt, capped at 10 s).
const TRAY_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Spawn the system tray indicator.
///
/// Runs in the background and updates the icon whenever the daemon state changes.
/// Retries with exponential backoff if the SNI host isn't available yet (common
/// on boot when the daemon starts before the desktop environment is fully ready).
pub async fn spawn_tray(
    state_rx: watch::Receiver<State>,
    #[cfg(feature = "recording")] recording_rx: watch::Receiver<RecordingStatus>,
    cmd_tx: mpsc::Sender<Command>,
    notify: Option<NotifyFn>,
    profiles: ProfileMenu,
    restart: RestartFn,
) {
    // Retry spawning the tray with exponential backoff.
    let mut delay = TRAY_INITIAL_DELAY;
    let mut handle = None;

    for attempt in 1..=TRAY_MAX_RETRIES {
        let tray = WhisrsTray {
            state: TrayState {
                current: *state_rx.borrow(),
                #[cfg(feature = "recording")]
                recording: recording_rx.borrow().clone(),
            },
            cmd_tx: cmd_tx.clone(),
            notify,
            profiles: profiles.clone(),
            restart: std::sync::Arc::clone(&restart),
        };

        match tray.spawn().await {
            Ok(h) => {
                info!("system tray started (attempt {attempt})");
                handle = Some(h);
                break;
            }
            Err(e) => {
                if attempt == TRAY_MAX_RETRIES {
                    warn!(
                        "failed to start system tray after {TRAY_MAX_RETRIES} attempts: {e} — continuing without tray"
                    );
                    return;
                }
                info!(
                    "tray host not available (attempt {attempt}/{TRAY_MAX_RETRIES}): {e} — retrying in {delay:?}"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(10));
            }
        }
    }

    let handle = handle.expect("handle must be set after successful spawn");

    tokio::spawn(watch_updates(
        handle,
        state_rx,
        #[cfg(feature = "recording")]
        recording_rx,
    ));
}

/// Show a new dictation state. Mutating the tray object itself makes ksni
/// emit the corresponding D-Bus property changes for title, tooltip, and
/// icon pixmap.
async fn show_state(handle: &ksni::Handle<WhisrsTray>, new_state: State) {
    debug!("tray state update: {new_state:?}");
    handle
        .update(|tray| {
            tray.state.current = new_state;
        })
        .await;
}

/// Watch for dictation changes and update the tray, until the channel
/// closes (the daemon is going away).
#[cfg(not(feature = "recording"))]
async fn watch_updates(handle: ksni::Handle<WhisrsTray>, mut state_rx: watch::Receiver<State>) {
    while state_rx.changed().await.is_ok() {
        let new_state = *state_rx.borrow();
        show_state(&handle, new_state).await;
    }
}

/// Watch for dictation and recording changes and update the tray. The
/// dictation channel ends the loop when it closes (the daemon is going
/// away); a closed recording channel only stops recording updates.
#[cfg(feature = "recording")]
async fn watch_updates(
    handle: ksni::Handle<WhisrsTray>,
    mut state_rx: watch::Receiver<State>,
    mut recording_rx: watch::Receiver<RecordingStatus>,
) {
    let mut recording_open = true;
    loop {
        tokio::select! {
            changed = state_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let new_state = *state_rx.borrow();
                show_state(&handle, new_state).await;
            }
            changed = recording_rx.changed(), if recording_open => {
                if changed.is_err() {
                    recording_open = false;
                    continue;
                }
                let recording = recording_rx.borrow().clone();
                debug!("tray recording update: {:?}", recording.phase);
                handle
                    .update(|tray| {
                        tray.state.recording = recording;
                    })
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Brings the `activate` trait method into scope for method-call syntax
    // below; `impl ksni::Tray for WhisrsTray` above only qualifies the path,
    // it doesn't import the trait.
    use ksni::Tray as _;

    /// A tray in dictation state `current`, with no recording.
    fn tray_with(cmd_tx: mpsc::Sender<Command>, current: State) -> WhisrsTray {
        WhisrsTray {
            state: TrayState {
                current,
                #[cfg(feature = "recording")]
                recording: RecordingStatus::default(),
            },
            cmd_tx,
            notify: None,
            profiles: ProfileMenu::default(),
            restart: std::sync::Arc::new(|_| panic!("a tray test restarted the daemon")),
        }
    }

    /// Dictation's words in the title, the menu's status line and the
    /// tooltip never say "record", the recording feature's word, nor use an
    /// em dash.
    #[test]
    fn dictation_states_never_say_record() {
        let (cmd_tx, _rx) = mpsc::channel(1);
        for state in [
            State::Idle,
            State::Recording,
            State::Transcribing,
            State::Synthesizing,
            State::Speaking,
        ] {
            let tray = tray_with(cmd_tx.clone(), state);
            for text in [
                tray.title(),
                tray.tool_tip().description,
                state_word(state).to_string(),
            ] {
                assert!(!text.to_lowercase().contains("record"), "{state:?}: {text}");
                assert!(!text.contains('\u{2014}'), "{state:?}: {text}");
            }
        }
        let dictating = tray_with(cmd_tx, State::Recording);
        assert_eq!(dictating.tool_tip().description, "Dictating...");
        assert_eq!(dictating.title(), "whisrs: dictating");
    }

    /// No two icon sets share a pixmap at any size, so a recording never looks
    /// like a dictation state (or the other recording phase) on the panel.
    #[test]
    fn every_icon_set_is_distinct_at_every_size() {
        let sets: Vec<(IconSet, Vec<Icon>)> =
            IconSet::ALL.iter().map(|&s| (s, icons::get(s))).collect();
        for (i, (a, a_icons)) in sets.iter().enumerate() {
            for (b, b_icons) in &sets[i + 1..] {
                for (x, y) in a_icons.iter().zip(b_icons) {
                    assert_ne!(x.data, y.data, "{a:?} and {b:?} match at {}px", x.width);
                }
            }
        }
    }

    /// `IconSet::ALL` lists every set in declaration order, which is what
    /// `icons::get` indexes its cache by.
    #[test]
    fn icon_set_cache_index_is_the_declaration_order() {
        for (i, set) in IconSet::ALL.iter().enumerate() {
            assert_eq!(*set as usize, i, "{set:?}");
        }
    }

    /// `activate()` (left-click on the tray icon) is the only tray-driven
    /// command, and nothing checks that it still sends `Toggle` after a
    /// refactor — a typo'd variant here would ship silently. Pin it.
    #[test]
    fn activate_queues_toggle_command() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
        let mut tray = tray_with(cmd_tx, State::Idle);

        tray.activate(0, 0);

        let cmd = cmd_rx
            .try_recv()
            .expect("activate() should queue a command");
        assert!(
            matches!(cmd, Command::Toggle { language: None }),
            "expected Command::Toggle {{ language: None }}, got {cmd:?}"
        );
    }

    /// "Restart Daemon" goes through the daemon's own restart, handing it
    /// what to do when nothing restarted.
    #[test]
    fn restart_daemon_uses_the_daemons_restart() {
        let (cmd_tx, _rx) = mpsc::channel(1);
        let mut tray = tray_with(cmd_tx, State::Idle);
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce(String) + Send>>();
        let tx = std::sync::Mutex::new(tx);
        tray.restart = std::sync::Arc::new(move |on_failure| {
            tx.lock().unwrap().send(on_failure).unwrap();
        });
        let mut menu = tray.menu();
        let item = menu
            .iter_mut()
            .find_map(|i| match i {
                MenuItem::Standard(s) if s.label == "Restart Daemon" => Some(s),
                _ => None,
            })
            .expect("a Restart Daemon item");
        let activate = std::mem::replace(&mut item.activate, Box::new(|_: &mut WhisrsTray| {}));
        activate(&mut tray);
        let on_failure = rx.try_recv().expect("the daemon's restart ran");
        // With notifications off, a failure is only logged.
        on_failure("`systemctl --user restart --no-block whisrs.service` failed".into());
    }

    /// Without profiles there is no "Profile" submenu, and the tooltip names
    /// none.
    #[test]
    fn no_profile_submenu_without_profiles() {
        let (cmd_tx, _rx) = mpsc::channel(1);
        let tray = tray_with(cmd_tx, State::Idle);
        assert!(tray
            .menu()
            .iter()
            .all(|i| !matches!(i, MenuItem::SubMenu(_) | MenuItem::RadioGroup(_))));
        assert!(!tray.tool_tip().description.contains("profile"));
    }

    /// With profiles, the submenu offers base and each of them, the daemon's
    /// one selected; picking one sends its switch. The tooltip names it.
    #[test]
    fn the_profile_submenu_offers_base_and_the_profiles() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
        let mut tray = tray_with(cmd_tx, State::Idle);
        tray.profiles = ProfileMenu {
            names: vec!["my_laptop".into(), "offline".into()],
            active: Some("offline".into()),
        };
        let menu = tray.menu();
        let submenu = menu
            .iter()
            .find_map(|i| match i {
                MenuItem::SubMenu(s) if s.label == "Profile" => Some(s),
                _ => None,
            })
            .expect("a Profile submenu");
        let [MenuItem::RadioGroup(group)] = submenu.submenu.as_slice() else {
            panic!("the submenu is one radio group");
        };
        let labels: Vec<&str> = group.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["base", "my__laptop", "offline"]);
        assert_eq!(group.selected, 2);

        for (index, name) in [(0, "base"), (1, "my_laptop")] {
            (group.select)(&mut tray, index);
            let cmd = cmd_rx.try_recv().expect("a switch is queued");
            assert!(
                matches!(&cmd, Command::SwitchProfile { name: Some(n) } if n == name),
                "{cmd:?}"
            );
        }
        assert_eq!(
            tray.tool_tip().description,
            "Idle, ready to dictate\nprofile: offline"
        );

        tray.profiles.active = None;
        let menu = tray.menu();
        let selected = menu.iter().find_map(|i| match i {
            MenuItem::SubMenu(s) => match s.submenu.as_slice() {
                [MenuItem::RadioGroup(g)] => Some(g.selected),
                _ => None,
            },
            _ => None,
        });
        assert_eq!(selected, Some(0), "base");
        assert_eq!(tray.tool_tip().description, "Idle, ready to dictate");
    }

    /// Every icon set (each dictation state and both recording phases) ships
    /// every size, and each decodes to a full ARGB pixmap that is not blank.
    /// Catches a missing or mis-rendered PNG at test time rather than as a
    /// panic in the running tray.
    #[test]
    fn every_state_has_all_icon_sizes() {
        for state in [
            State::Idle,
            State::Recording,
            State::Transcribing,
            State::Synthesizing,
            State::Speaking,
        ] {
            assert!(IconSet::ALL.contains(&IconSet::for_state(state)));
        }
        for set in IconSet::ALL {
            let icons = icons::get(set);
            let sizes: Vec<i32> = icons.iter().map(|i| i.width).collect();
            assert_eq!(sizes, [16, 22, 32, 48], "{set:?}");
            for icon in &icons {
                assert_eq!(icon.width, icon.height, "{set:?} icon is square");
                assert_eq!(
                    icon.data.len(),
                    (icon.width * icon.height * 4) as usize,
                    "{set:?} {}px pixmap length",
                    icon.width
                );
                assert!(
                    icon.data.as_chunks::<4>().0.iter().any(|p| p[0] > 0),
                    "{set:?} {}px icon is fully transparent",
                    icon.width
                );
            }
        }
    }

    /// Without the recording feature the tray is dictation's alone: no
    /// recording item in the menu and only the five dictation icon sets.
    #[cfg(not(feature = "recording"))]
    #[test]
    fn no_recording_items_or_icons_without_the_feature() {
        let (cmd_tx, _rx) = mpsc::channel(1);
        let tray = tray_with(cmd_tx, State::Idle);
        let labels: Vec<String> = tray
            .menu()
            .iter()
            .filter_map(|i| match i {
                MenuItem::Standard(s) => Some(s.label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 3, "{labels:?}");
        assert!(labels[0].starts_with("whisrs v"), "{labels:?}");
        assert_eq!(labels[1..], ["Restart Daemon", "Quit"]);
        assert_eq!(IconSet::ALL.len(), 5);
        assert!(!tray
            .tool_tip()
            .description
            .to_lowercase()
            .contains("record"));
    }

    /// The recording items, icons and lines, in a build with the feature.
    #[cfg(feature = "recording")]
    mod recording {
        use super::*;

        /// A tray in dictation state `current` while `recording` runs.
        fn tray_with(
            cmd_tx: mpsc::Sender<Command>,
            current: State,
            recording: RecordingStatus,
        ) -> WhisrsTray {
            let mut tray = super::tray_with(cmd_tx, current);
            tray.state.recording = recording;
            tray
        }

        fn active_recording(title: Option<&str>) -> RecordingStatus {
            use chrono::TimeZone;
            RecordingStatus {
                phase: RecordingPhase::Recording,
                id: Some("2026-10-02_0930".to_string()),
                title: title.map(str::to_string),
                started_at: Some(
                    chrono::Local
                        .with_ymd_and_hms(2026, 10, 2, 9, 30, 0)
                        .unwrap(),
                ),
                ..Default::default()
            }
        }

        fn labels(items: &[MenuItem<WhisrsTray>]) -> Vec<(String, bool)> {
            items
                .iter()
                .filter_map(|i| match i {
                    MenuItem::Standard(s) => Some((s.label.clone(), s.enabled)),
                    _ => None,
                })
                .collect()
        }

        /// Activate the standard menu item with this label.
        fn click(tray: &mut WhisrsTray, label: &str) {
            let mut items = recording_menu_items(&tray.state.recording);
            let item = items
                .iter_mut()
                .find_map(|i| match i {
                    MenuItem::Standard(s) if s.label == label => Some(s),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no menu item `{label}`"));
            let activate = std::mem::replace(&mut item.activate, Box::new(|_: &mut WhisrsTray| {}));
            activate(tray);
        }

        #[test]
        fn recording_line_describes_each_phase() {
            assert_eq!(recording_line(&RecordingStatus::default()), None);
            assert_eq!(
                recording_line(&active_recording(Some("standup"))).as_deref(),
                Some("Recording mic and system audio since 09:30 (standup)")
            );
            assert_eq!(
                recording_line(&active_recording(None)).as_deref(),
                Some("Recording mic and system audio since 09:30")
            );
            let processing = RecordingStatus {
                phase: RecordingPhase::Processing,
                ..Default::default()
            };
            assert_eq!(
                recording_line(&processing).as_deref(),
                Some("Processing recording...")
            );
        }

        #[test]
        fn idle_recording_menu_offers_only_start() {
            assert_eq!(
                labels(&recording_menu_items(&RecordingStatus::default())),
                vec![("Start recording (mic + system audio)".to_string(), true)]
            );
        }

        #[test]
        fn active_recording_menu_offers_status_stop_and_cancel() {
            assert_eq!(
                labels(&recording_menu_items(&active_recording(None))),
                vec![
                    (
                        "Recording mic and system audio since 09:30".to_string(),
                        false
                    ),
                    ("Stop recording and transcribe".to_string(), true),
                    ("Cancel recording and delete it".to_string(), true),
                ]
            );
        }

        #[test]
        fn recording_menu_items_send_toggle_and_cancel() {
            let (cmd_tx, mut cmd_rx) = mpsc::channel(4);
            let mut tray = tray_with(cmd_tx, State::Idle, active_recording(None));

            click(&mut tray, "Stop recording and transcribe");
            click(&mut tray, "Cancel recording and delete it");

            for expected in [RecordingAction::Toggle, RecordingAction::Cancel] {
                let cmd = cmd_rx.try_recv().expect("a recording command is queued");
                assert!(
                    matches!(
                        &cmd,
                        Command::Recording { action, title: None, id: None, options }
                            if *action == expected && *options == RecordingOptions::default()
                    ),
                    "expected recording {expected:?}, got {cmd:?}"
                );
            }
        }

        /// Every size's pixmap, for comparing whole icon sets.
        fn pixmaps(icons: &[Icon]) -> Vec<&[u8]> {
            icons.iter().map(|i| i.data.as_slice()).collect()
        }

        /// The recording shows in the icon and tooltip only while dictation is
        /// idle; a dictation in progress keeps its own icon, and the recording is
        /// still named in the tooltip.
        #[test]
        fn dictation_state_wins_the_icon_during_a_recording() {
            let (cmd_tx, _rx) = mpsc::channel(1);
            let idle = tray_with(cmd_tx.clone(), State::Idle, active_recording(Some("x")));
            assert_eq!(
                pixmaps(&idle.icon_pixmap()),
                pixmaps(&icons::get(IconSet::RecordingActive))
            );
            assert_eq!(
                idle.tool_tip().description,
                "Recording mic and system audio since 09:30 (x)"
            );

            let processing = RecordingStatus {
                phase: RecordingPhase::Processing,
                ..Default::default()
            };
            let idle_processing = tray_with(cmd_tx.clone(), State::Idle, processing.clone());
            assert_eq!(
                pixmaps(&idle_processing.icon_pixmap()),
                pixmaps(&icons::get(IconSet::RecordingProcessing))
            );

            let dictating = tray_with(
                cmd_tx.clone(),
                State::Recording,
                active_recording(Some("x")),
            );
            assert_eq!(
                pixmaps(&dictating.icon_pixmap()),
                pixmaps(&icons::get(IconSet::Recording))
            );
            assert!(dictating
                .tool_tip()
                .description
                .ends_with("Recording mic and system audio since 09:30 (x)"));

            let transcribing = tray_with(cmd_tx, State::Transcribing, processing);
            assert_eq!(
                pixmaps(&transcribing.icon_pixmap()),
                pixmaps(&icons::get(IconSet::Transcribing))
            );
        }
    }
}
