//! System tray implementation using ksni (StatusNotifierItem).

use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, ToolTip, TrayMethods};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use super::NotifyFn;
use crate::service::{RestartOutcome, ServiceManager};
use crate::{Command, State};

/// Speech-bubble tray icons, one set per state, at several sizes so HiDPI
/// panels get a sharp render instead of an upscaled 16x16.
///
/// The PNGs in `src/tray/icons/` are rendered from the SVGs next to them:
/// `rsvg-convert -w N -h N <state>.svg -o <state>-N.png` for each size.
mod icons {
    use std::sync::OnceLock;

    use ksni::Icon;

    use crate::State;

    macro_rules! state_pngs {
        ($name:literal) => {
            [
                include_bytes!(concat!("icons/", $name, "-16.png")).as_slice(),
                include_bytes!(concat!("icons/", $name, "-22.png")).as_slice(),
                include_bytes!(concat!("icons/", $name, "-32.png")).as_slice(),
                include_bytes!(concat!("icons/", $name, "-48.png")).as_slice(),
            ]
        };
    }

    fn pngs(state: State) -> [&'static [u8]; 4] {
        match state {
            State::Idle => state_pngs!("idle"),
            State::Recording => state_pngs!("recording"),
            State::Transcribing => state_pngs!("transcribing"),
            State::Synthesizing => state_pngs!("synthesizing"),
            State::Speaking => state_pngs!("speaking"),
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

    /// All sizes for `state`, decoded once and cached for the daemon's lifetime.
    pub fn for_state(state: State) -> Vec<Icon> {
        static CACHE: OnceLock<[Vec<Icon>; 5]> = OnceLock::new();
        let cache = CACHE.get_or_init(|| {
            [
                State::Idle,
                State::Recording,
                State::Transcribing,
                State::Synthesizing,
                State::Speaking,
            ]
            .map(|s| pngs(s).iter().map(|b| decode(b)).collect())
        });
        let index = match state {
            State::Idle => 0,
            State::Recording => 1,
            State::Transcribing => 2,
            State::Synthesizing => 3,
            State::Speaking => 4,
        };
        cache[index].clone()
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
}

/// Human-readable word for a state, used in the title and menu status line.
fn state_word(state: State) -> &'static str {
    match state {
        State::Idle => "idle",
        State::Recording => "recording",
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
        format!("whisrs — {}", state_word(self.state.current))
    }

    /// Left-click on the icon: toggle recording, same as `whisrs toggle`.
    fn activate(&mut self, _x: i32, _y: i32) {
        debug!("tray activated (left-click): toggle");
        self.send(Command::Toggle { language: None });
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        icons::for_state(self.state.current)
    }

    fn tool_tip(&self) -> ToolTip {
        let description = match self.state.current {
            State::Idle => "Idle — ready to record",
            State::Recording => "Recording...",
            State::Transcribing => "Transcribing...",
            State::Synthesizing => "Synthesizing…",
            State::Speaking => "Reading aloud…",
        };
        ToolTip {
            title: "whisrs".to_string(),
            description: description.to_string(),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    /// Right-click menu.
    ///
    /// Deliberately holds no recording controls: whisrs is driven by hotkeys,
    /// and toggle/cancel each already have a hotkey, a CLI command, and (for
    /// toggle) the left-click above. A menu item for them would be a fourth
    /// way to do the same thing. What is left is what has no other trigger.
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = self.state.current;
        vec![
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
            MenuItem::Standard(StandardItem {
                label: "Restart Daemon".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    restart_daemon(tray.notify);
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
        ]
    }
}

/// Restart the daemon through the detected service manager, from the tray menu.
///
/// This cannot go through the daemon's own command loop: a successful restart
/// kills this very process before it could reply. Runs on its own thread
/// because ksni menu callbacks must not block and the restart is a subprocess
/// round-trip.
///
/// Failure raises a desktop toast (when `notify` is set): without one, a click
/// on a setup with no installed service would silently do nothing, since log
/// warnings are invisible from the tray.
fn restart_daemon(notify: Option<NotifyFn>) {
    info!("tray: restart requested");
    std::thread::spawn(move || {
        let manager = ServiceManager::detect();
        match manager.restart() {
            // When this daemon runs under the service, the manager kills it
            // mid-restart, so this line is normally never reached.
            RestartOutcome::Restarted => {
                info!("tray: daemon restarted via {}", manager.name())
            }
            RestartOutcome::NoService => {
                warn!("tray: no whisrs user service installed — restart the daemon manually");
                if let Some(notify) = notify {
                    notify(
                        "whisrs",
                        "Restart from the tray needs an installed whisrs user service. \
                         Restart the daemon manually.",
                    );
                }
            }
            RestartOutcome::Failed => {
                let hint = manager.restart_hint().unwrap_or("the restart command");
                warn!("tray: `{hint}` failed");
                if let Some(notify) = notify {
                    notify(
                        "whisrs",
                        &format!("Daemon restart failed: `{hint}` returned an error."),
                    );
                }
            }
        }
    });
}

/// Quit from the tray menu, mirroring the daemon's SIGINT handler:
/// remove the IPC socket, then exit cleanly.
fn quit_daemon() {
    info!("tray: quit requested, shutting down");
    let _ = std::fs::remove_file(crate::socket_path());
    std::process::exit(0);
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
    mut state_rx: watch::Receiver<State>,
    cmd_tx: mpsc::Sender<Command>,
    notify: Option<NotifyFn>,
) {
    // Retry spawning the tray with exponential backoff.
    let mut delay = TRAY_INITIAL_DELAY;
    let mut handle = None;

    for attempt in 1..=TRAY_MAX_RETRIES {
        let tray = WhisrsTray {
            state: TrayState {
                current: *state_rx.borrow(),
            },
            cmd_tx: cmd_tx.clone(),
            notify,
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

    // Watch for state changes and update the tray.
    tokio::spawn(async move {
        while state_rx.changed().await.is_ok() {
            let new_state = *state_rx.borrow();
            debug!("tray state update: {new_state:?}");
            // Mutate the tray object itself so ksni emits the corresponding
            // D-Bus property changes for title, tooltip, and icon pixmap.
            handle
                .update(|tray| {
                    tray.state.current = new_state;
                })
                .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    // Brings the `activate` trait method into scope for method-call syntax
    // below; `impl ksni::Tray for WhisrsTray` above only qualifies the path,
    // it doesn't import the trait.
    use ksni::Tray as _;

    /// `activate()` (left-click on the tray icon) is the only tray-driven
    /// command, and nothing checks that it still sends `Toggle` after a
    /// refactor — a typo'd variant here would ship silently. Pin it.
    #[test]
    fn activate_queues_toggle_command() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
        let mut tray = WhisrsTray {
            state: TrayState {
                current: State::Idle,
            },
            cmd_tx,
            notify: None,
        };

        tray.activate(0, 0);

        let cmd = cmd_rx
            .try_recv()
            .expect("activate() should queue a command");
        assert!(
            matches!(cmd, Command::Toggle { language: None }),
            "expected Command::Toggle {{ language: None }}, got {cmd:?}"
        );
    }

    /// Every state ships every size, and each decodes to a full ARGB pixmap
    /// that is not blank. Catches a missing or mis-rendered PNG at test time
    /// rather than as a panic in the running tray.
    #[test]
    fn every_state_has_all_icon_sizes() {
        for state in [
            State::Idle,
            State::Recording,
            State::Transcribing,
            State::Synthesizing,
            State::Speaking,
        ] {
            let set = icons::for_state(state);
            let sizes: Vec<i32> = set.iter().map(|i| i.width).collect();
            assert_eq!(sizes, [16, 22, 32, 48], "{state:?}");
            for icon in &set {
                assert_eq!(icon.width, icon.height, "{state:?} icon is square");
                assert_eq!(
                    icon.data.len(),
                    (icon.width * icon.height * 4) as usize,
                    "{state:?} {}px pixmap length",
                    icon.width
                );
                assert!(
                    icon.data.as_chunks::<4>().0.iter().any(|p| p[0] > 0),
                    "{state:?} {}px icon is fully transparent",
                    icon.width
                );
            }
        }
    }
}
