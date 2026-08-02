//! Desktop overlay shown while recording or transcribing.

use std::sync::mpsc;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::{OverlayConfig, State};

use super::gnome::{is_gnome_desktop, run_gnome_broadcaster};
use super::wayland::run_overlay;
use super::x11::run_x11_overlay;

#[derive(Debug, thiserror::Error)]
pub(super) enum OverlayError {
    #[error("Wayland connection error: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("Wayland globals error: {0}")]
    Globals(#[from] wayland_client::globals::GlobalError),
    #[error("smithay bind error: {0}")]
    Bind(#[from] wayland_client::globals::BindError),
    #[error("smithay shm create error: {0}")]
    Shm(#[from] smithay_client_toolkit::shm::CreatePoolError),
    #[error("Wayland dispatch error: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("X11 display connect error: {0}")]
    X11Connect(#[from] x11rb::errors::ConnectError),
    #[error("X11 protocol/IO error: {0}")]
    X11Connection(#[from] x11rb::errors::ConnectionError),
    #[error("X11 reply error: {0}")]
    X11Reply(#[from] x11rb::errors::ReplyError),
    #[error("X11 reply/id error: {0}")]
    X11ReplyOrId(#[from] x11rb::errors::ReplyOrIdError),
    #[error("X11 ARGB visual not available")]
    X11Visual,
    #[error("D-Bus error: {0}")]
    DBus(#[from] zbus::Error),
    #[error("D-Bus signal error: {0}")]
    DBusSignal(#[from] zbus::fdo::Error),
    #[error("tiny-skia pixmap allocation failed for {0}x{1}")]
    Pixmap(u32, u32),
}

/// Spawn the bottom recording overlay.
///
/// Native compositor event loops run on a dedicated OS thread because they are
/// blocking client loops. A small Tokio task forwards daemon state changes into
/// that thread.
pub async fn spawn_overlay(
    mut state_rx: watch::Receiver<State>,
    mut level_rx: watch::Receiver<f32>,
    config: OverlayConfig,
) {
    if is_gnome_desktop() {
        let gnome_state_rx = state_rx.clone();
        let gnome_level_rx = level_rx.clone();
        let gnome_theme = config.theme.clone();
        tokio::spawn(async move {
            if let Err(e) = run_gnome_broadcaster(gnome_state_rx, gnome_level_rx, gnome_theme).await
            {
                warn!("GNOME overlay D-Bus broadcaster unavailable: {e:#}");
            }
        });
    }

    let (tx, rx) = mpsc::channel::<State>();
    let (level_tx, level_rx_thread) = mpsc::channel::<f32>();

    let backend = OverlayBackend::detect();
    info!("overlay backend selected: {backend:?}");
    let overlay_config = config;
    std::thread::Builder::new()
        .name("whisrs-overlay".to_string())
        .spawn(move || {
            let result = match backend {
                OverlayBackend::Wayland => run_overlay(rx, level_rx_thread, overlay_config),
                OverlayBackend::X11 => run_x11_overlay(rx, level_rx_thread, overlay_config),
                OverlayBackend::Unavailable => {
                    warn!("overlay unavailable: no Wayland or X11 display in environment");
                    return;
                }
            };
            if let Err(e) = result {
                warn!("overlay unavailable: {e:#}");
            }
        })
        .map_err(|e| warn!("failed to spawn overlay thread: {e}"))
        .ok();

    tokio::spawn(async move {
        let _ = tx.send(*state_rx.borrow());
        let _ = level_tx.send(*level_rx.borrow());
        loop {
            tokio::select! {
                changed = state_rx.changed() => {
                    if changed.is_err() { break; }
                    if tx.send(*state_rx.borrow()).is_err() { break; }
                }
                changed = level_rx.changed() => {
                    if changed.is_err() { break; }
                    let _ = level_tx.send(*level_rx.borrow());
                }
            }
        }
    });
}

#[derive(Debug, Clone, Copy)]
enum OverlayBackend {
    Wayland,
    X11,
    Unavailable,
}

impl OverlayBackend {
    fn detect() -> Self {
        let session_is_wayland = matches_env("XDG_SESSION_TYPE", "wayland");
        let session_is_x11 = matches_env("XDG_SESSION_TYPE", "x11");
        if (env_var_is_set("WAYLAND_DISPLAY") || session_is_wayland) && !session_is_x11 {
            Self::Wayland
        } else if env_var_is_set("DISPLAY") {
            Self::X11
        } else {
            Self::Unavailable
        }
    }
}

fn env_var_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn matches_env(name: &str, expected: &str) -> bool {
    std::env::var(name).is_ok_and(|value| value.eq_ignore_ascii_case(expected))
}
