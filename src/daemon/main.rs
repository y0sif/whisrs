use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::AsyncWriteExt;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use whisrs::history;
use whisrs::window::{self, WindowTracker};
use whisrs::{encode_message, read_message, socket_path, Command, DaemonInfo, Response};

mod agent;
mod command_mode;
mod context;
mod dictation;
mod factory;
#[cfg(feature = "hooks")]
mod hooks;
mod injection;
mod notify;
mod pipeline;
mod profile;
#[cfg(feature = "recording")]
mod recording;
mod selection;
mod speak;
mod startup;

use crate::command_mode::{handle_command_mode, handle_llm_command, handle_set_llm_instruction};
use crate::context::{DaemonContext, DaemonState};
use crate::dictation::{handle_cancel, handle_toggle};
use crate::factory::create_backend;
use crate::injection::{prime_modifier_probe, warm_keyboard};
use crate::notify::send_notification;
#[cfg(feature = "recording")]
use crate::recording::{error_already_notified, handle_recording, RecordingRuntime};
use crate::speak::{handle_speak, handle_speak_text};
#[cfg(feature = "hooks")]
use crate::startup::check_session_bus;
use crate::startup::{
    bind_socket, check_audio_devices, check_uinput_access, cleanup_stale_socket,
    import_compositor_env, load_config, validate_config,
};

/// The daemon takes no options of its own, but declaring the interface means
/// `--version` and `--help` are answered and exit, instead of being ignored and
/// starting a daemon.
#[derive(Parser)]
#[command(name = "whisrsd", about = "whisrs dictation daemon", version)]
struct Args {}

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
    // Not dead code: this call is what makes the flags above exit.
    Args::parse();
    // Taken first, so `info` says when this process started.
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);

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

    let startup::LoadedConfig {
        config,
        warning: config_warning,
        profile,
        profile_error,
    } = load_config();
    validate_config(&config);
    check_audio_devices(&config.audio.device);
    let notify = config.general.notify;

    // Notify user if config parsing failed and defaults are being used, or
    // the profile did not load.
    if let Some(msg) = config_warning {
        if notify {
            send_notification("whisrs", &msg);
        }
    }

    let backend = create_backend(&config);

    // Wait for compositor environment on boot (WAYLAND_DISPLAY, etc.).
    // Must run before window tracker detection and any clipboard operations.
    import_compositor_env().await;
    // Off the startup path: the prewarm sleeps 1 s so X11 can attach the
    // device keymap. It holds the keyboard lock throughout, so an early
    // injection waits for it and reuses that keyboard.
    {
        let key_delay = std::time::Duration::from_millis(config.input.key_delay_ms);
        let backend = config.input.backend;
        tokio::task::spawn_blocking(move || warm_keyboard(key_delay, backend));
    }
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

    // State broadcast: hooks and the agent reply queue read the state as it
    // is; the tray and the pill overlay read it without agent mode, which
    // shows on its own overlay (`StateBus`).
    let state_tx = crate::context::StateBus::new();
    let (overlay_level_tx, overlay_level_rx) = tokio::sync::watch::channel(0.0_f32);
    // Recording status has its own channel, read by the tray only: a recording
    // must not animate the overlay or fire the dictation hooks.
    #[cfg(feature = "recording")]
    let (recording_runtime, recording_rx, recording_queue_rx) = RecordingRuntime::new();

    let tray_enabled = config.general.tray;
    let overlay_enabled = config.general.overlay;
    let overlay_config = config.overlay.clone().unwrap_or_default();
    // Inert (no file, no task, no process) without [[agents]].
    let agents = crate::agent::AgentRuntime::new(&config, &socket_path());
    // The agent overlay exists only with agents; it follows `[general]
    // overlay` unless `[agent_overlay] enabled` says otherwise. Either
    // overlay gets the mic and TTS levels.
    let agent_overlay_config = config.agent_overlay.clone().unwrap_or_default();
    let agent_overlay_enabled =
        agents.is_enabled() && agent_overlay_config.enabled(overlay_enabled);
    // The tray's Profile submenu: the profiles there are now.
    let profile_menu = whisrs::tray::ProfileMenu {
        names: profile_names(),
        active: profile.clone(),
    };
    let context = Arc::new(DaemonContext {
        config,
        window_tracker,
        transcription_backend: backend,
        notify,
        state_tx,
        overlay_level_tx: (overlay_enabled || agent_overlay_enabled).then_some(overlay_level_tx),
        overlay_enabled,
        #[cfg(feature = "recording")]
        recording: recording_runtime,
        agents,
        agent_board: tokio::sync::watch::channel(Vec::new()).0,
        agent_overlay_shown: std::sync::atomic::AtomicBool::new(false),
        started_at,
        profile: crate::profile::ProfileRuntime::new(profile, profile_error),
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
                info!("internal command (hotkey/tray): {}", cmd.log_summary());
                // A press shows nobody the answer, so a refusal while a
                // profile switch is under way is notified.
                if let Some(message) = crate::profile::switching_refusal(&dispatch_ctx, &cmd) {
                    info!("{} refused: {message}", cmd.log_summary());
                    if dispatch_ctx.notify_error() {
                        send_notification("whisrs", &message);
                    }
                    continue;
                }
                // A recording start can spend seconds opening devices; run it
                // aside so a dictation hotkey pressed meanwhile is not held
                // behind it. Recordings never touch the dictation state, so
                // there is nothing to broadcast after it. Without the feature
                // nothing internal sends one (no hotkey, no tray item).
                #[cfg(feature = "recording")]
                if let Command::Recording {
                    action,
                    title,
                    id,
                    options,
                } = cmd
                {
                    let ctx = Arc::clone(&dispatch_ctx);
                    tokio::spawn(async move {
                        let notify = ctx.notify_error();
                        if let Response::Error { message } =
                            handle_recording(ctx, action, title, id, options).await
                        {
                            // A stop that saved but could not queue the
                            // recording has notified already.
                            if notify && !error_already_notified(&message) {
                                send_notification("whisrs", &format!("Recording: {message}"));
                            }
                        }
                    });
                    continue;
                }
                // A hotkey press shows nobody the response, so an agent
                // error is notified here (unless it already was), and so is a
                // refused profile switch.
                let is_agent = matches!(cmd, Command::Agent { .. });
                let is_switch = matches!(cmd, Command::SwitchProfile { .. });
                let response =
                    handle_command(cmd, Arc::clone(&dispatch_state), Arc::clone(&dispatch_ctx))
                        .await;
                if let Response::Error { message } = &response {
                    if is_agent
                        && dispatch_ctx.notify_error()
                        && !crate::agent::error_already_notified(message)
                    {
                        send_notification("whisrs", &format!("Agent: {message}"));
                    }
                    if is_switch && dispatch_ctx.notify_error() {
                        send_notification("whisrs", message);
                    }
                }
                // Broadcast state for tray.
                dispatch_ctx.state_tx.publish(&*dispatch_state.lock().await);
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
        // "Restart Daemon" restarts as a profile switch does.
        let tray_restart: whisrs::tray::RestartFn = {
            let context = Arc::clone(&context);
            Arc::new(move |on_failure| context.profile.restart(Arc::clone(&context), on_failure))
        };
        tokio::spawn(whisrs::tray::spawn_tray(
            context.state_tx.subscribe_ui(),
            #[cfg(feature = "recording")]
            recording_rx,
            cmd_tx.clone(),
            tray_notify,
            profile_menu,
            tray_restart,
        ));
    }

    // Recording-lifecycle hooks (MPRIS pause + shell commands). Watches the
    // raw state broadcast, so an agent's voice turn pauses media too;
    // detached, so failures never delay recording.
    #[cfg(feature = "hooks")]
    if let Some(hooks) = context.config.hooks.clone() {
        tokio::spawn(crate::hooks::hook_dispatch_loop(
            context.state_tx.subscribe_raw(),
            hooks,
        ));
    }

    // The recording pill and the agent overlay, if enabled, each on a
    // thread of its own so desktop integration failures do not stop the
    // daemon. Without agents there is no agent overlay. While it is on
    // screen it stands in for the agent progress toasts.
    if overlay_enabled || agent_overlay_enabled {
        let feeds = whisrs::overlay::OverlayFeeds {
            pill: overlay_enabled.then(|| whisrs::overlay::PillFeed {
                state_rx: context.state_tx.subscribe_ui(),
                level_rx: overlay_level_rx.clone(),
                config: overlay_config,
            }),
            agents: agent_overlay_enabled.then(|| whisrs::overlay::AgentsFeed {
                entries_rx: context.agent_board.subscribe(),
                level_rx: overlay_level_rx.clone(),
                config: agent_overlay_config,
            }),
        };
        let mut shown = whisrs::overlay::spawn_overlays(feeds);
        if agent_overlay_enabled {
            let shown_ctx = Arc::clone(&context);
            tokio::spawn(async move {
                loop {
                    let on = shown.get();
                    shown_ctx
                        .agent_overlay_shown
                        .store(on, std::sync::atomic::Ordering::Relaxed);
                    tracing::debug!(
                        "agent overlay {}",
                        if on {
                            "on screen: agent progress toasts off"
                        } else {
                            "not on screen: agent progress toasts on"
                        }
                    );
                    if !shown.changed().await {
                        break;
                    }
                }
            });
        }
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

    let listener = bind_socket(&sock_path)?;
    info!("listening on {}", sock_path.display());
    // What a re-exec (a profile switch) set aside, now that the socket is
    // this daemon's.
    let _ = std::fs::remove_file(whisrs::service::socket_aside(&sock_path));

    // Only now, with the socket ours: a second whisrsd that exits at the
    // check above must not also recover and process the same recordings.
    // Recording processing runs on its own task, one recording at a time, so a
    // long transcription never holds up IPC or dictation. Recovery of
    // recordings a crash or shutdown interrupted is spawned too, so the socket
    // serves without waiting for WAV repair. Commands that queued a recording
    // before this wait in the channel.
    #[cfg(feature = "recording")]
    {
        let worker = tokio::spawn(crate::recording::run_worker(
            Arc::clone(&context),
            recording_queue_rx,
        ));
        // Shutdown aborts it, so a SIGTERM mid-transcription is not counted as
        // a crashed attempt.
        context.recording.set_worker(worker);
        tokio::spawn(crate::recording::recover_on_startup(Arc::clone(&context)));
        // Recordings that could not reach the backend are tried again every
        // few minutes while the daemon runs.
        tokio::spawn(crate::recording::run_network_retries(Arc::clone(&context)));
    }

    // Agent mode: the reply queue, the agent overlay's board, and the turns
    // a restart interrupted (watched again so their replies are still
    // spoken). Only with agents. Without them, what an earlier daemon's
    // turns left in their scopes is still stopped (it died mid-turn, then
    // `[[agents]]` was emptied).
    if context.agents.is_enabled() {
        crate::agent::start_background(Arc::clone(&context), Arc::clone(&daemon_state)).await;
    } else {
        crate::agent::sweep_without_agents();
    }

    // SIGTERM (systemd stop/restart, tray Quit) and SIGINT both remove the
    // socket before exiting, and finalize a recording in progress, so it is
    // transcribed on the next start.
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
    let sock_path_clone = sock_path.clone();
    let shutdown_ctx = Arc::clone(&context);
    tokio::spawn(async move {
        let name = tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigint.recv() => "SIGINT",
        };
        info!("received {name}, shutting down");
        shut_down(&shutdown_ctx).await;
        let _ = std::fs::remove_file(&sock_path_clone);
        std::process::exit(0);
    });

    // Start global hotkey listener if configured — the fixed [hotkeys]
    // section, any [[llm_commands]] entry, or any [[agents]] entry with a
    // hotkey or a new_session_hotkey (each carries its own hotkeys
    // independent of [hotkeys]).
    // Spawned as a background task so retries don't block the IPC server.
    if context.config.hotkeys.is_some()
        || !context.config.llm_commands.is_empty()
        || context.agents.has_hotkeys()
    {
        let hk_config = context.config.hotkeys.clone().unwrap_or_default();
        let llm_commands = context.config.llm_commands.clone();
        let agents = context.agents.agents();
        let hk_tx = cmd_tx.clone();
        tokio::spawn(async move {
            // Hotkey presses feed the shared dispatch loop above.
            whisrs::hotkey::start_hotkey_listener(&hk_config, &llm_commands, &agents, hk_tx).await;
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
    // The TUI asks `info` every 2 s while it is open, and `recording
    // status` as often while its Recordings screen shows one in flight.
    if matches!(cmd, Command::Info) || is_recording_status(&cmd) {
        debug!("received command: {}", cmd.log_summary());
    } else {
        info!("received command: {}", cmd.log_summary());
    }

    // A profile switch changes no dictation state either, and its answer
    // must go out before the restart it schedules: no wait for the lock.
    let skip_publish = is_read_only(&cmd) || matches!(cmd, Command::SwitchProfile { .. });
    let response = handle_command(cmd, Arc::clone(&daemon_state), Arc::clone(&context)).await;

    // Broadcast state for tray updates. A read-only command changed nothing
    // to broadcast, and the TUI's `info` poll must not wait for the
    // dictation lock here either.
    if !skip_publish {
        context.state_tx.publish(&*daemon_state.lock().await);
    }

    let encoded = encode_message(&response)?;
    writer.write_all(&encoded).await?;
    writer.shutdown().await?;
    Ok(())
}

/// Whether `cmd` only reads: it changes no state, so there is nothing of its
/// own to broadcast after it. A broadcast there could show sooner a
/// transition that was not published (the command-mode and LLM-command
/// claims), but nothing waits on that: the tray and the overlays catch up at
/// the next publish, and the hooks and the reply queue are woken by changes.
fn is_read_only(cmd: &Command) -> bool {
    matches!(cmd, Command::Status | Command::Info | Command::Log { .. }) || is_recording_status(cmd)
}

/// `recording status`, which only reads the recording view.
fn is_recording_status(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Recording {
            action: whisrs::recording::RecordingAction::Status,
            ..
        }
    )
}

/// What a stop does before the process ends, on SIGTERM or SIGINT and
/// before a re-exec: a recording in progress is finalized, so the next start
/// transcribes it.
pub(crate) async fn shut_down(_context: &DaemonContext) {
    #[cfg(feature = "recording")]
    crate::recording::shutdown(_context).await;
}

async fn handle_command(
    cmd: Command,
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) -> Response {
    if let Some(message) = crate::profile::switching_refusal(&context, &cmd) {
        info!("{} refused: {message}", cmd.log_summary());
        return Response::Error { message };
    }
    match cmd {
        Command::Toggle { language } => handle_toggle(daemon_state, context, language).await,
        Command::Cancel => handle_cancel(daemon_state, context).await,
        Command::Status => {
            let ds = daemon_state.lock().await;
            Response::Ok {
                state: ds.state_machine.state(),
            }
        }
        // Never waits on a lock: a poll every 2 s would time out on each
        // dictation start (`handle_toggle` holds the lock through the mic
        // open). Free, the lock gives the real state, unpublished claims
        // included; held, the published one, and `busy`. `busy` also covers
        // a held agent runtime and a recording start or stop in progress.
        Command::Info => {
            let (state, locked) = match daemon_state.try_lock() {
                Ok(ds) => (ds.state_machine.state(), false),
                Err(_) => (context.state_tx.raw(), true),
            };
            let (agent_turn, agents_locked) = match context.agents.any_turn_running() {
                Some(running) => (running, false),
                None => (false, true),
            };
            #[cfg(feature = "recording")]
            let recording_busy = context.recording.session_busy();
            #[cfg(not(feature = "recording"))]
            let recording_busy = false;
            let mut info = daemon_info(state, &context);
            info.busy = locked || agents_locked || recording_busy;
            info.agent_turn = agent_turn;
            Response::Info { info }
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
        Command::SpeakText { text } => handle_speak_text(daemon_state, context, text).await,
        Command::Agent {
            action,
            name,
            text,
            new,
        } => crate::agent::handle_agent(daemon_state, context, action, name, text, new).await,
        Command::AgentEvent { payload } => {
            crate::agent::handle_agent_event(&context, payload).await;
            Response::Ok {
                state: daemon_state.lock().await.state_machine.state(),
            }
        }
        #[cfg(feature = "recording")]
        Command::Recording {
            action,
            title,
            id,
            options,
        } => handle_recording(context, action, title, id, options).await,
        // The wire keeps the command, so a CLI built with the feature gets
        // a plain answer from a daemon built without it.
        #[cfg(not(feature = "recording"))]
        Command::Recording { action, .. } => {
            info!("recording {action:?} refused: built without the recording feature");
            Response::Error {
                message: RECORDING_MISSING.to_string(),
            }
        }
        Command::SwitchProfile { name } => {
            crate::profile::handle_switch_profile(daemon_state, context, name).await
        }
    }
}

/// The profiles there are, for the tray. Files that are no profile are
/// named in the log.
fn profile_names() -> Vec<String> {
    let dir = whisrs::config::profile::profiles_dir();
    match whisrs::config::profile::list(&dir) {
        Ok(listing) => {
            for skipped in &listing.skipped {
                tracing::warn!("{}", skipped.warning());
            }
            listing.names
        }
        Err(e) => {
            tracing::warn!("cannot read {}: {e}", dir.display());
            Vec::new()
        }
    }
}

/// The answer to `info`: this whisrsd, its build, and what it does now
/// (`state` is the dictation state).
fn daemon_info(state: whisrs::State, context: &DaemonContext) -> DaemonInfo {
    #[cfg(feature = "recording")]
    let recording = context.recording.phase();
    #[cfg(not(feature = "recording"))]
    let recording = whisrs::recording::RecordingPhase::Idle;
    DaemonInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        state: Some(state),
        recording: Some(recording),
        profile: context.profile.active.clone(),
        profile_error: context.profile.error(),
        backend: context.config.general.backend.clone(),
        features: whisrs::built_features(),
        started_at: context.started_at,
        busy: false,
        agent_turn: false,
    }
}

/// What a daemon built without the `recording` feature answers to every
/// `recording` command.
#[cfg(not(feature = "recording"))]
const RECORDING_MISSING: &str = "this whisrsd was built without the recording feature, so it has \
     no recordings to work with. Use a release build, or rebuild with `cargo install whisrs \
     --features recording`.";

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

    /// `contrib/whisrs.service` runs `whisrsd` with no arguments, so a
    /// required field or positional on `Args` would break every install.
    #[test]
    fn daemon_parses_with_no_arguments() {
        assert!(Args::try_parse_from(["whisrsd"]).is_ok());
    }

    /// A daemon context for the `info` tests, its backend `deepgram`.
    fn info_context() -> Arc<DaemonContext> {
        Arc::new(DaemonContext {
            config: toml::from_str("[general]\nbackend = \"deepgram\"\n").expect("fixture parses"),
            window_tracker: Arc::new(whisrs::window::NoopTracker),
            // Never called: nothing is transcribed.
            transcription_backend: Arc::new(whisrs::transcription::groq::GroqBackend::new(
                String::new(),
            )),
            notify: false,
            state_tx: crate::context::StateBus::new(),
            overlay_level_tx: None,
            overlay_enabled: false,
            #[cfg(feature = "recording")]
            recording: crate::recording::RecordingRuntime::new().0,
            agents: crate::agent::AgentRuntime::disabled(),
            agent_board: tokio::sync::watch::channel(Vec::new()).0,
            agent_overlay_shown: std::sync::atomic::AtomicBool::new(false),
            started_at: 1_793_000_000_000,
            profile: crate::profile::ProfileRuntime::inert(),
        })
    }

    /// `info` names this whisrsd, its build and its config's backend, beside
    /// the state `status` reports.
    #[tokio::test]
    async fn info_reports_this_daemon() {
        let daemon_state = Arc::new(Mutex::new(DaemonState::new()));
        let response = handle_command(Command::Info, daemon_state, info_context()).await;
        let Response::Info { info } = response else {
            panic!("expected an info response, got {response:?}");
        };
        assert_eq!(
            info,
            DaemonInfo {
                version: env!("CARGO_PKG_VERSION").to_string(),
                state: Some(whisrs::State::Idle),
                recording: Some(whisrs::recording::RecordingPhase::Idle),
                profile: None,
                profile_error: None,
                backend: "deepgram".to_string(),
                features: whisrs::built_features(),
                started_at: 1_793_000_000_000,
                busy: false,
                agent_turn: false,
            }
        );
    }

    /// `info` names the profile the daemon runs on, or why the active one
    /// did not load.
    #[tokio::test]
    async fn info_names_the_profile() {
        let why = "profile offline did not load, using base: profiles/offline.toml does not exist";
        for (active, error) in [(Some("offline"), None), (None, Some(why))] {
            let mut context = Arc::into_inner(info_context()).expect("one owner");
            context.profile = crate::profile::ProfileRuntime::with(
                whisrs::config::profile::Paths::under(std::path::Path::new("/nonexistent")),
                active.map(String::from),
                error.map(String::from),
                Arc::new(|_, _| panic!("info restarted the daemon")),
            );
            let daemon_state = Arc::new(Mutex::new(DaemonState::new()));
            let response = handle_command(Command::Info, daemon_state, Arc::new(context)).await;
            let Response::Info { info } = response else {
                panic!("expected an info response, got {response:?}");
            };
            assert_eq!(info.profile.as_deref(), active);
            assert_eq!(info.profile_error.as_deref(), error);
        }
    }

    /// `info` answers while the dictation lock is held (`handle_toggle` holds
    /// it through the mic open), with the state last published, and busy.
    #[tokio::test]
    async fn info_answers_while_the_dictation_lock_is_held() {
        use whisrs::state::Action;

        let context = info_context();
        let daemon_state = Arc::new(Mutex::new(DaemonState::new()));
        let mut held = daemon_state.lock().await;
        held.state_machine.transition(Action::Toggle).unwrap();
        context.state_tx.publish(&held);

        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle_command(Command::Info, Arc::clone(&daemon_state), context),
        )
        .await
        .expect("info waited for the dictation lock");
        drop(held);
        let Response::Info { info } = answer else {
            panic!("expected an info response, got {answer:?}");
        };
        assert_eq!(info.state, Some(whisrs::State::Recording));
        assert!(info.busy);
    }

    /// With the lock free, `info` reads the state machine itself: a claim
    /// that moved it without publishing shows, and nothing is busy.
    #[tokio::test]
    async fn info_reads_an_unpublished_state_under_a_free_lock() {
        use whisrs::state::Action;

        let context = info_context();
        let mut ds = DaemonState::new();
        ds.state_machine.transition(Action::Toggle).unwrap();
        ds.state_machine.transition(Action::Toggle).unwrap();
        let daemon_state = Arc::new(Mutex::new(ds));
        assert_eq!(context.state_tx.raw(), whisrs::State::Idle);

        let answer = handle_command(Command::Info, daemon_state, context).await;
        let Response::Info { info } = answer else {
            panic!("expected an info response, got {answer:?}");
        };
        assert_eq!(info.state, Some(whisrs::State::Transcribing));
        assert!(!info.busy);
        assert!(!info.agent_turn);
    }

    /// Only the commands that change nothing skip the broadcast after them.
    #[test]
    fn only_reads_skip_the_broadcast() {
        assert!(is_read_only(&Command::Status));
        assert!(is_read_only(&Command::Info));
        assert!(is_read_only(&Command::Log { limit: 5 }));
        let recording = |action| Command::Recording {
            action,
            title: None,
            id: None,
            options: Default::default(),
        };
        assert!(is_read_only(&recording(
            whisrs::recording::RecordingAction::Status
        )));
        for changes in [
            recording(whisrs::recording::RecordingAction::Delete),
            recording(whisrs::recording::RecordingAction::Toggle),
            Command::Toggle { language: None },
            Command::Cancel,
            Command::ClearHistory,
            Command::CommandMode,
            Command::Speak,
            Command::SwitchProfile { name: None },
        ] {
            assert!(!is_read_only(&changes), "{}", changes.log_summary());
        }
    }

    /// Built without the recording feature, every `recording` command,
    /// straight off the wire, gets the same plain error, and dictation's
    /// state is left alone.
    #[cfg(not(feature = "recording"))]
    #[tokio::test]
    async fn recording_commands_are_refused_without_the_feature() {
        use whisrs::recording::RecordingAction;
        use whisrs::State;

        let context = Arc::new(DaemonContext {
            config: toml::from_str("").expect("empty config uses defaults"),
            window_tracker: Arc::new(whisrs::window::NoopTracker),
            // Never called: nothing is transcribed.
            transcription_backend: Arc::new(whisrs::transcription::groq::GroqBackend::new(
                String::new(),
            )),
            notify: false,
            state_tx: crate::context::StateBus::new(),
            overlay_level_tx: None,
            overlay_enabled: false,
            agents: crate::agent::AgentRuntime::disabled(),
            agent_board: tokio::sync::watch::channel(Vec::new()).0,
            agent_overlay_shown: std::sync::atomic::AtomicBool::new(false),
            started_at: 0,
            profile: crate::profile::ProfileRuntime::inert(),
        });
        let daemon_state = Arc::new(Mutex::new(DaemonState::new()));
        for action in [
            RecordingAction::Start,
            RecordingAction::Stop,
            RecordingAction::Cancel,
            RecordingAction::Status,
            RecordingAction::Toggle,
            RecordingAction::Retry,
            RecordingAction::Copy,
            RecordingAction::Name,
            RecordingAction::Summarize,
            RecordingAction::Delete,
        ] {
            let json = serde_json::to_string(&serde_json::json!({
                "cmd": "recording",
                "action": action,
                "title": "Weekly sync",
            }))
            .unwrap();
            let cmd: Command = serde_json::from_str(&json).expect("the wire still has it");
            let response =
                handle_command(cmd, Arc::clone(&daemon_state), Arc::clone(&context)).await;
            match response {
                Response::Error { message } => assert_eq!(
                    message,
                    "this whisrsd was built without the recording feature, so it has no \
                     recordings to work with. Use a release build, or rebuild with `cargo \
                     install whisrs --features recording`.",
                    "{action:?}"
                ),
                other => panic!("{action:?}: expected an error, got {other:?}"),
            }
        }
        assert_eq!(daemon_state.lock().await.state_machine.state(), State::Idle);
    }
}
