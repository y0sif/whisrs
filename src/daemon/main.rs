use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info};

use whisrs::history;
use whisrs::window::{self, WindowTracker};
use whisrs::{encode_message, read_message, socket_path, Command, Response, State};

mod command_mode;
mod context;
mod dictation;
mod factory;
#[cfg(feature = "hooks")]
mod hooks;
mod injection;
mod notify;
mod pipeline;
mod quiet;
mod selection;
mod speak;
mod startup;

use crate::command_mode::{handle_command_mode, handle_llm_command, handle_set_llm_instruction};
use crate::context::{DaemonContext, DaemonState};
use crate::dictation::{handle_cancel, handle_toggle};
use crate::factory::create_backend;
use crate::injection::{prime_modifier_probe, warm_keyboard};
use crate::notify::send_notification;
use crate::speak::handle_speak;
#[cfg(feature = "hooks")]
use crate::startup::check_session_bus;
use crate::startup::{
    check_audio_devices, check_uinput_access, cleanup_stale_socket, import_compositor_env,
    load_config, validate_config,
};

/// Declaring the interface also means `--version` and `--help` are answered
/// and exit, instead of being ignored and starting a daemon.
#[derive(Parser)]
#[command(name = "whisrsd", about = "whisrs dictation daemon", version)]
struct Args {
    /// Log startup only. Once the first command arrives, nothing more is
    /// written to stdout or stderr (the journal under systemd).
    #[arg(short, long)]
    quiet: bool,
}

/// The pulseaudio crate logs every connection, and an ERROR on each normal
/// disconnect, so it is silenced unless `RUST_LOG` names it.
const DEFAULT_LOG_DIRECTIVES: &str = "info,pulseaudio=off";

/// Log filter directives: `RUST_LOG` when set, with `pulseaudio=off` appended
/// unless it already mentions `pulseaudio`.
fn log_directives(rust_log: Option<&str>) -> String {
    match rust_log.map(str::trim).filter(|v| !v.is_empty()) {
        None => DEFAULT_LOG_DIRECTIVES.to_string(),
        Some(v) if v.contains("pulseaudio") => v.to_string(),
        Some(v) => format!("{v},pulseaudio=off"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.quiet {
        quiet::enable();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(log_directives(
                std::env::var("RUST_LOG").ok().as_deref(),
            ))
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_DIRECTIVES)),
        )
        .init();

    info!("whisrsd v{} starting", env!("CARGO_PKG_VERSION"));

    check_uinput_access();

    let (config, config_warning) = load_config();
    validate_config(&config);
    check_audio_devices(&config.audio.device);
    let notify = config.general.notify;

    // Notify user if config parsing failed and defaults are being used.
    if let Some(msg) = config_warning {
        if notify {
            send_notification("whisrs", &msg);
        }
    }

    let backend = create_backend(&config);

    // Wait for compositor environment on boot (WAYLAND_DISPLAY, etc.).
    // Must run before window tracker detection and any clipboard operations.
    import_compositor_env().await;
    warm_keyboard(
        std::time::Duration::from_millis(config.input.key_delay_ms),
        config.input.backend,
    );
    // Open the keyboards the pre-injection modifier check reads (#154) now,
    // off the hot path, so the first dictation does not pay for it. Always:
    // selection capture (speak, command) checks modifiers even when
    // clipboard_only skips typing.
    tokio::task::spawn_blocking(prime_modifier_probe);

    // Check D-Bus session bus if MPRIS media pause is configured.
    #[cfg(feature = "hooks")]
    if config.hooks.as_ref().is_some_and(|h| h.media_auto_pause) {
        check_session_bus().await;
    }

    let window_tracker: Arc<dyn WindowTracker> = Arc::from(window::detect_tracker());
    info!(
        "window tracker: {}",
        std::any::type_name_of_val(&*window_tracker)
    );

    // State broadcast channel — consumed by system tray and overlay.
    let (state_tx, state_rx) = tokio::sync::watch::channel(State::Idle);
    let (overlay_level_tx, overlay_level_rx) = tokio::sync::watch::channel(0.0_f32);

    let tray_enabled = config.general.tray;
    let overlay_enabled = config.general.overlay;
    let overlay_config = config.overlay.clone().unwrap_or_default();
    let context = Arc::new(DaemonContext {
        config,
        window_tracker,
        transcription_backend: backend,
        notify,
        state_tx,
        overlay_level_tx: overlay_enabled.then_some(overlay_level_tx),
        overlay_enabled,
    });

    let daemon_state = Arc::new(Mutex::new(DaemonState::new()));

    // Shared command channel for in-process command sources (hotkeys, tray
    // menu). One dispatch loop drains it and routes every command through
    // `handle_command`, exactly like commands arriving over the IPC socket.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<Command>(16);
    {
        let dispatch_state = Arc::clone(&daemon_state);
        let dispatch_ctx = Arc::clone(&context);
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                quiet::enter_runtime();
                info!("internal command (hotkey/tray): {cmd:?}");
                let _response =
                    handle_command(cmd, Arc::clone(&dispatch_state), Arc::clone(&dispatch_ctx))
                        .await;
                // Broadcast state for tray.
                let current = dispatch_state.lock().await.state_machine.state();
                let _ = dispatch_ctx.state_tx.send(current);
            }
        });
    }

    // Start system tray if enabled.
    // Spawned as a background task so retries don't block the IPC server.
    if tray_enabled {
        // Hand the tray the daemon's toast helper so menu failures (a failed
        // "Restart Daemon" click) reach the user, not just the journal. Gated
        // like every other error notification.
        let tray_notify = context
            .notify_error()
            .then_some(send_notification as whisrs::tray::NotifyFn);
        tokio::spawn(whisrs::tray::spawn_tray(
            state_rx.clone(),
            cmd_tx.clone(),
            tray_notify,
        ));
    }

    // Recording-lifecycle hooks (MPRIS pause + shell commands). Watches the
    // same state broadcast as the tray/overlay; detached, so failures never
    // delay recording.
    #[cfg(feature = "hooks")]
    if let Some(hooks) = context.config.hooks.clone() {
        tokio::spawn(crate::hooks::hook_dispatch_loop(state_rx.clone(), hooks));
    }

    // Start bottom recording overlay if enabled.
    // Spawned as a background task so desktop integration failures do not stop the daemon.
    if overlay_enabled {
        tokio::spawn(whisrs::overlay::spawn_overlay(
            state_rx,
            overlay_level_rx,
            overlay_config,
        ));
    }

    let sock_path = socket_path();
    info!("socket path: {}", sock_path.display());

    if let Some(parent) = sock_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }

    cleanup_stale_socket(&sock_path).await?;

    let listener = UnixListener::bind(&sock_path).context("failed to bind Unix socket")?;
    info!("listening on {}", sock_path.display());

    let sock_path_clone = sock_path.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("received SIGINT, shutting down");
        let _ = std::fs::remove_file(&sock_path_clone);
        std::process::exit(0);
    });

    // Start global hotkey listener if configured — either the fixed
    // [hotkeys] section, or any [[llm_commands]] entry (each carries its own
    // hotkey independent of [hotkeys]).
    // Spawned as a background task so retries don't block the IPC server.
    if context.config.hotkeys.is_some() || !context.config.llm_commands.is_empty() {
        let hk_config = context.config.hotkeys.clone().unwrap_or_default();
        let llm_commands = context.config.llm_commands.clone();
        let hk_tx = cmd_tx.clone();
        tokio::spawn(async move {
            // Hotkey presses feed the shared dispatch loop above.
            whisrs::hotkey::start_hotkey_listener(&hk_config, &llm_commands, hk_tx).await;
        });
    }

    loop {
        let (stream, _addr) = listener.accept().await?;
        let state = Arc::clone(&daemon_state);
        let ctx = Arc::clone(&context);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state, ctx).await {
                error!("connection error: {e:#}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let cmd: Command = read_message(&mut reader).await?;
    quiet::enter_runtime();
    info!("received command: {cmd:?}");

    let response = handle_command(cmd, Arc::clone(&daemon_state), Arc::clone(&context)).await;

    // Broadcast state for tray updates.
    let current = daemon_state.lock().await.state_machine.state();
    let _ = context.state_tx.send(current);

    let encoded = encode_message(&response)?;
    writer.write_all(&encoded).await?;
    writer.shutdown().await?;
    Ok(())
}

async fn handle_command(
    cmd: Command,
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) -> Response {
    match cmd {
        Command::Toggle { language } => handle_toggle(daemon_state, context, language).await,
        Command::Cancel => handle_cancel(daemon_state, context).await,
        Command::Status => {
            let ds = daemon_state.lock().await;
            Response::Ok {
                state: ds.state_machine.state(),
            }
        }
        Command::Log { limit } => match history::read_entries(limit) {
            Ok(entries) => Response::History { entries },
            Err(e) => Response::Error {
                message: format!("failed to read history: {e}"),
            },
        },
        Command::ClearHistory => match history::clear_history() {
            Ok(()) => {
                info!("transcription history cleared");
                Response::Ok {
                    state: daemon_state.lock().await.state_machine.state(),
                }
            }
            Err(e) => Response::Error {
                message: format!("failed to clear history: {e}"),
            },
        },
        Command::CommandMode => handle_command_mode(daemon_state, context).await,
        Command::LlmCommand { name } => handle_llm_command(daemon_state, context, name).await,
        Command::SetLlmInstruction { name } => {
            handle_set_llm_instruction(daemon_state, context, name).await
        }
        Command::Speak => handle_speak(daemon_state, context).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{error::ErrorKind, CommandFactory};

    /// Catches a malformed arg definition — duplicate names, conflicting
    /// shorts — the moment `Args` grows a real flag.
    #[test]
    fn daemon_args_are_valid() {
        Args::command().debug_assert();
    }

    #[test]
    fn daemon_quiet_flag_parses() {
        assert!(!Args::try_parse_from(["whisrsd"]).unwrap().quiet);
        assert!(Args::try_parse_from(["whisrsd", "-q"]).unwrap().quiet);
        assert!(Args::try_parse_from(["whisrsd", "--quiet"]).unwrap().quiet);
    }

    /// `Args` exists only so `--version` and `--help` are answered and exit
    /// instead of being ignored and starting a daemon. Pin that behaviour,
    /// since nothing else in the daemon would notice if it regressed.
    ///
    /// `try_parse_from` rather than `parse`, which would exit the test harness.
    #[test]
    fn daemon_answers_version_and_help_instead_of_starting() {
        let Err(err) = Args::try_parse_from(["whisrsd", "--version"]) else {
            panic!("--version parsed as a normal start instead of exiting");
        };
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));

        let Err(err) = Args::try_parse_from(["whisrsd", "--help"]) else {
            panic!("--help parsed as a normal start instead of exiting");
        };
        assert_eq!(err.kind(), ErrorKind::DisplayHelp);
    }

    #[test]
    fn daemon_rejects_unknown_flags() {
        assert!(Args::try_parse_from(["whisrsd", "--nope"]).is_err());
    }

    #[test]
    fn log_directives_silence_pulseaudio() {
        assert_eq!(log_directives(None), "info,pulseaudio=off");
        assert_eq!(log_directives(Some("  ")), "info,pulseaudio=off");
        assert_eq!(log_directives(Some("debug")), "debug,pulseaudio=off");
        assert_eq!(
            log_directives(Some("whisrs=trace,pulseaudio=warn")),
            "whisrs=trace,pulseaudio=warn"
        );
    }

    /// Units written before `--quiet` run `whisrsd` with no arguments, so a
    /// required field or positional on `Args` would break those installs.
    #[test]
    fn daemon_parses_with_no_arguments() {
        assert!(Args::try_parse_from(["whisrsd"]).is_ok());
    }
}
