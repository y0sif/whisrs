use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use whisrs::audio::capture::AudioCaptureHandle;
use whisrs::audio::silence::AutoStopDetector;
use whisrs::state::{Action, StateMachine};
use whisrs::transcription::groq::GroqBackend;
use whisrs::transcription::local_parakeet::ParakeetBackend;
use whisrs::transcription::local_vosk::VoskBackend;
use whisrs::transcription::local_whisper::LocalWhisperBackend;
use whisrs::transcription::openai_realtime::OpenAIRealtimeBackend;
use whisrs::transcription::openai_rest::OpenAIRestBackend;
use whisrs::transcription::{TranscriptionBackend, TranscriptionConfig};
use whisrs::window::{self, WindowTracker};
use whisrs::{encode_message, read_message, socket_path, Command, Config, Response, State};

/// Shared daemon state protected by a mutex.
struct DaemonState {
    state_machine: StateMachine,
    audio_capture: Option<AudioCaptureHandle>,
    /// The window that was focused when recording started.
    recording_window_id: Option<String>,
    /// Handle to the background streaming pipeline (if active).
    streaming_task: Option<tokio::task::JoinHandle<Result<String>>>,
}

impl DaemonState {
    fn new() -> Self {
        Self {
            state_machine: StateMachine::new(),
            audio_capture: None,
            recording_window_id: None,
            streaming_task: None,
        }
    }
}

/// Resources shared across all connections (not behind the per-request mutex).
struct DaemonContext {
    config: Config,
    window_tracker: Box<dyn WindowTracker>,
    transcription_backend: Arc<dyn TranscriptionBackend>,
    notify: bool,
}

/// Try to connect to an existing socket.
async fn socket_is_alive(path: &std::path::Path) -> bool {
    tokio::net::UnixStream::connect(path).await.is_ok()
}

/// Remove a stale socket file if no daemon is listening on it.
async fn cleanup_stale_socket(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        if socket_is_alive(path).await {
            anyhow::bail!("another whisrsd instance is already running");
        }
        warn!("removing stale socket at {}", path.display());
        std::fs::remove_file(path).context("failed to remove stale socket")?;
    }
    Ok(())
}

/// Load configuration from config.toml, falling back to defaults.
fn load_config() -> Config {
    let config_path = whisrs::config_path();
    if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(contents) => match toml::from_str::<Config>(&contents) {
                Ok(config) => {
                    info!("loaded config from {}", config_path.display());
                    return config;
                }
                Err(e) => {
                    warn!("failed to parse config at {}: {e}", config_path.display());
                }
            },
            Err(e) => {
                warn!("failed to read config at {}: {e}", config_path.display());
            }
        }
    } else {
        info!(
            "no config file found at {}; using defaults",
            config_path.display()
        );
    }
    Config {
        general: Default::default(),
        audio: Default::default(),
        groq: None,
        openai: None,
        local_whisper: None,
        local_vosk: None,
        local_parakeet: None,
    }
}

fn check_uinput_access() {
    use std::fs::OpenOptions;
    match OpenOptions::new().write(true).open("/dev/uinput") {
        Ok(_) => info!("uinput access: ok"),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                warn!(
                    "Cannot open /dev/uinput — permission denied.\n\
                     Fix: sudo usermod -aG input $USER\n\
                          # Then log out and log back in\n\
                     Or install the udev rule:\n\
                          sudo cp contrib/99-whisrs.rules /etc/udev/rules.d/\n\
                          sudo udevadm control --reload-rules\n\
                          sudo udevadm trigger"
                );
            } else {
                warn!("Cannot open /dev/uinput: {e}");
            }
        }
    }
}

fn check_audio_devices() {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    match host.default_input_device() {
        Some(device) => {
            let name = device.name().unwrap_or_else(|_| "unknown".into());
            info!("default audio input device: {name}");
        }
        None => {
            warn!("no default audio input device found");
            if let Ok(devices) = host.input_devices() {
                let names: Vec<String> = devices.filter_map(|d| d.name().ok()).collect();
                if names.is_empty() {
                    warn!("no audio input devices available at all");
                } else {
                    warn!("available audio input devices: {}", names.join(", "));
                }
            }
        }
    }
}

fn validate_config(config: &Config) {
    match config.validate() {
        Ok(warnings) => {
            for w in &warnings {
                warn!("config: {}", w);
            }
        }
        Err(e) => error!("config: {e}"),
    }
    if !config.has_any_backend_configured() {
        warn!("No transcription backend configured. Run 'whisrs setup' to get started.");
    }
}

fn resolve_groq_api_key(config: &Config) -> Option<String> {
    if let Ok(key) = std::env::var("WHISRS_GROQ_API_KEY") {
        if !key.is_empty() {
            return Some(key);
        }
    }
    config.groq.as_ref().map(|g| g.api_key.clone())
}

fn resolve_openai_api_key(config: &Config) -> Option<String> {
    if let Ok(key) = std::env::var("WHISRS_OPENAI_API_KEY") {
        if !key.is_empty() {
            return Some(key);
        }
    }
    config.openai.as_ref().map(|o| o.api_key.clone())
}

fn create_backend(config: &Config) -> Arc<dyn TranscriptionBackend> {
    match config.general.backend.as_str() {
        "groq" => {
            let api_key = resolve_groq_api_key(config).unwrap_or_default();
            if api_key.is_empty() {
                warn!("no Groq API key configured");
            }
            info!("using Groq transcription backend");
            Arc::new(GroqBackend::new(api_key))
        }
        "openai-realtime" => {
            let api_key = resolve_openai_api_key(config).unwrap_or_default();
            if api_key.is_empty() {
                warn!("no OpenAI API key configured");
            }
            info!("using OpenAI Realtime transcription backend");
            Arc::new(OpenAIRealtimeBackend::new(api_key))
        }
        "openai" => {
            let api_key = resolve_openai_api_key(config).unwrap_or_default();
            if api_key.is_empty() {
                warn!("no OpenAI API key configured");
            }
            info!("using OpenAI REST transcription backend");
            Arc::new(OpenAIRestBackend::new(api_key))
        }
        "local-whisper" | "local" => {
            let model_path = config
                .local_whisper
                .as_ref()
                .map(|l| l.model_path.clone())
                .unwrap_or_else(|| {
                    dirs::data_dir()
                        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
                        .join("whisrs/models/ggml-base.en.bin")
                        .to_string_lossy()
                        .to_string()
                });
            info!("using local whisper transcription backend (model: {model_path})");
            Arc::new(LocalWhisperBackend::new(model_path))
        }
        "local-vosk" => {
            let model_path = config
                .local_vosk
                .as_ref()
                .map(|l| l.model_path.clone())
                .unwrap_or_else(|| {
                    dirs::data_dir()
                        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
                        .join("whisrs/models/vosk-model-small-en-us-0.15")
                        .to_string_lossy()
                        .to_string()
                });
            info!("using Vosk transcription backend (model: {model_path})");
            Arc::new(VoskBackend::new(model_path))
        }
        "local-parakeet" => {
            let model_path = config
                .local_parakeet
                .as_ref()
                .map(|l| l.model_path.clone())
                .unwrap_or_else(|| {
                    dirs::data_dir()
                        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
                        .join("whisrs/models/parakeet-eou-120m")
                        .to_string_lossy()
                        .to_string()
                });
            info!("using Parakeet transcription backend (model: {model_path})");
            Arc::new(ParakeetBackend::new(model_path))
        }
        other => {
            warn!("unknown backend '{other}', falling back to groq");
            let api_key = resolve_groq_api_key(config).unwrap_or_default();
            Arc::new(GroqBackend::new(api_key))
        }
    }
}

fn get_model_for_backend(config: &Config) -> String {
    match config.general.backend.as_str() {
        "groq" => config
            .groq
            .as_ref()
            .map(|g| g.model.clone())
            .unwrap_or_else(|| "whisper-large-v3-turbo".to_string()),
        "openai-realtime" | "openai" => config
            .openai
            .as_ref()
            .map(|o| o.model.clone())
            .unwrap_or_else(|| "gpt-4o-mini-transcribe".to_string()),
        "local-whisper" | "local" => "base.en".to_string(),
        "local-vosk" => "small-en-us".to_string(),
        "local-parakeet" => "eou-120m".to_string(),
        _ => "whisper-large-v3-turbo".to_string(),
    }
}

fn send_notification(summary: &str, body: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .summary(summary)
        .body(body)
        .appname("whisrs")
        .timeout(notify_rust::Timeout::Milliseconds(2000))
        .show()
    {
        warn!("failed to send notification: {e}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("whisrsd v{} starting", env!("CARGO_PKG_VERSION"));

    check_uinput_access();
    check_audio_devices();

    let config = load_config();
    validate_config(&config);
    let notify = config.general.notify;

    let backend = create_backend(&config);

    let window_tracker = window::detect_tracker();
    info!(
        "window tracker: {}",
        std::any::type_name_of_val(&*window_tracker)
    );

    let context = Arc::new(DaemonContext {
        config,
        window_tracker,
        transcription_backend: backend,
        notify,
    });

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

    let daemon_state = Arc::new(Mutex::new(DaemonState::new()));

    let sock_path_clone = sock_path.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("received SIGINT, shutting down");
        let _ = std::fs::remove_file(&sock_path_clone);
        std::process::exit(0);
    });

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
    info!("received command: {cmd:?}");

    let response = handle_command(cmd, daemon_state, context).await;

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
        Command::Toggle => handle_toggle(daemon_state, context).await,
        Command::Cancel => handle_cancel(daemon_state, context).await,
        Command::Status => {
            let ds = daemon_state.lock().await;
            Response::Ok {
                state: ds.state_machine.state(),
            }
        }
    }
}

async fn handle_toggle(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) -> Response {
    let mut ds = daemon_state.lock().await;
    let current_state = ds.state_machine.state();

    match current_state {
        State::Idle => {
            // Capture focused window before recording.
            let window_id = match context.window_tracker.get_focused_window() {
                Ok(id) => {
                    info!("captured source window: {id}");
                    Some(id)
                }
                Err(e) => {
                    warn!("failed to capture focused window: {e}");
                    None
                }
            };

            // Start recording.
            let mut capture = match AudioCaptureHandle::start() {
                Ok(c) => c,
                Err(e) => {
                    let msg = format!("{e}");
                    let friendly = if msg.contains("no default audio input device") {
                        format_no_microphone_error()
                    } else {
                        format!("Failed to start audio capture: {e}")
                    };
                    error!("{friendly}");
                    return Response::Error { message: friendly };
                }
            };

            // For streaming backends: start the streaming pipeline immediately.
            // Audio flows in real-time from microphone → API → text at cursor.
            if context.transcription_backend.supports_streaming() {
                let audio_rx = match capture.take_receiver() {
                    Some(rx) => rx,
                    None => {
                        return Response::Error {
                            message: "failed to get audio receiver".to_string(),
                        }
                    }
                };

                let config = TranscriptionConfig {
                    language: context.config.general.language.clone(),
                    model: get_model_for_backend(&context.config),
                };

                let backend = Arc::clone(&context.transcription_backend);
                let wid = window_id.clone();
                let ctx_notify = context.notify;
                let window_tracker_ref = &context.window_tracker;
                // Restore focus before starting the pipeline.
                let wid_for_focus = wid.clone();

                // Spawn a task to:
                // 1. Run auto-stop detection + forward audio to transcription
                // 2. Run transcription backend
                // 3. Type text as it arrives
                let silence_timeout = context.config.general.silence_timeout_ms;
                let ds_ref = Arc::clone(&daemon_state);

                let task = tokio::spawn(async move {
                    run_streaming_pipeline(
                        audio_rx,
                        backend,
                        config,
                        wid,
                        ctx_notify,
                        silence_timeout,
                        ds_ref,
                    )
                    .await
                });

                ds.streaming_task = Some(task);

                // Focus the window now (so text goes to the right place from the start).
                if let Some(wid) = &wid_for_focus {
                    if let Err(e) = window_tracker_ref.focus_window(wid) {
                        warn!("failed to pre-focus window: {e}");
                    }
                }
            }

            ds.audio_capture = Some(capture);
            ds.recording_window_id = window_id;

            match ds.state_machine.transition(Action::Toggle) {
                Ok(new_state) => {
                    info!("started recording");
                    if context.notify {
                        send_notification("whisrs", "Recording...");
                    }
                    Response::Ok { state: new_state }
                }
                Err(e) => {
                    ds.audio_capture = None;
                    ds.recording_window_id = None;
                    ds.streaming_task = None;
                    Response::Error {
                        message: e.to_string(),
                    }
                }
            }
        }
        State::Recording => {
            // Stop recording.
            match ds.state_machine.transition(Action::Toggle) {
                Ok(_) => {
                    info!("stopped recording, transitioning to transcribing");
                    if context.notify {
                        send_notification("whisrs", "Transcribing...");
                    }

                    let capture = ds.audio_capture.take();
                    let window_id = ds.recording_window_id.take();
                    let streaming_task = ds.streaming_task.take();

                    // Release lock before slow operations.
                    drop(ds);

                    let result = if let Some(task) = streaming_task {
                        // Streaming path: stop capture to close the channel,
                        // then wait for the pipeline to drain and finish.
                        if let Some(mut cap) = capture {
                            cap.stop();
                            tokio::task::spawn_blocking(move || drop(cap));
                        }
                        match task.await {
                            Ok(Ok(text)) => Ok(text),
                            Ok(Err(e)) => Err(e),
                            Err(e) => Err(anyhow::anyhow!("streaming task panicked: {e}")),
                        }
                    } else {
                        // Batch path: collect all audio, then transcribe.
                        process_recording_batch(capture, window_id.as_deref(), &context).await
                    };

                    // Transition back to Idle.
                    let mut ds = daemon_state.lock().await;
                    match ds.state_machine.transition(Action::TranscriptionDone) {
                        Ok(new_state) => match result {
                            Ok(text) => {
                                info!("transcription complete: {} chars", text.len());
                                if context.notify {
                                    let preview = if text.len() > 80 {
                                        format!("{}...", &text[..77])
                                    } else {
                                        text.clone()
                                    };
                                    send_notification("whisrs", &format!("Done: {preview}"));
                                }
                                Response::Ok { state: new_state }
                            }
                            Err(e) => {
                                error!("transcription failed: {e:#}");
                                if context.notify {
                                    send_notification(
                                        "whisrs",
                                        &format!("Transcription failed: {e}"),
                                    );
                                }
                                Response::Ok { state: new_state }
                            }
                        },
                        Err(e) => Response::Error {
                            message: e.to_string(),
                        },
                    }
                }
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        State::Transcribing => Response::Error {
            message: "cannot toggle while transcribing".to_string(),
        },
    }
}

/// The streaming pipeline: reads audio in real-time, sends to API, types text.
/// Also monitors for silence auto-stop.
async fn run_streaming_pipeline(
    mut audio_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<i16>>,
    backend: Arc<dyn TranscriptionBackend>,
    config: TranscriptionConfig,
    window_id: Option<String>,
    notify: bool,
    silence_timeout_ms: u64,
    daemon_state: Arc<Mutex<DaemonState>>,
) -> Result<String> {
    let (audio_tx, backend_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(256);
    let (text_tx, mut text_rx) = tokio::sync::mpsc::channel::<String>(64);

    // Spawn the transcription backend.
    let config_clone = config.clone();
    let backend_task = tokio::spawn(async move {
        backend
            .transcribe_stream(backend_rx, text_tx, &config_clone)
            .await
    });

    // Spawn a task that types text as it arrives.
    let wid = window_id.clone();
    let typing_task = tokio::spawn(async move {
        let mut full_text = String::new();
        let mut first_text = true;

        while let Some(text) = text_rx.recv().await {
            if text.is_empty() {
                continue;
            }

            // On first text chunk, restore window focus.
            if first_text {
                if let Some(wid) = &wid {
                    let wid_clone = wid.clone();
                    // We can't access the window tracker here easily, but the
                    // focus was pre-set when recording started. If the user
                    // switched windows, we can't fix that without the tracker.
                    // The focus was set at recording start which is the expected behavior.
                    let _ = wid_clone; // focus already set
                }
                first_text = false;
            }

            // Determine what to type BEFORE updating full_text.
            let text_to_type = if full_text.is_empty() {
                text.clone()
            } else if !text.starts_with(' ') && !full_text.ends_with(' ') {
                format!(" {text}")
            } else {
                text.clone()
            };

            full_text.push_str(&text_to_type);

            info!("typing: {:?}", text_to_type);
            if let Err(e) =
                tokio::task::spawn_blocking(move || type_text_at_cursor(&text_to_type)).await
            {
                warn!("failed to type text: {e}");
            }
        }

        full_text
    });

    // Forward audio from capture to backend, with auto-stop detection.
    let mut auto_stop = AutoStopDetector::new(0.003, silence_timeout_ms, 16_000);

    while let Some(chunk) = audio_rx.recv().await {
        // Check for auto-stop.
        if auto_stop.feed(&chunk) {
            info!("silence auto-stop triggered after {silence_timeout_ms}ms");
            if notify {
                send_notification("whisrs", "Auto-stopped (silence detected)");
            }

            // Trigger stop: signal the daemon state machine.
            // We stop the audio capture by closing the forwarding channel.
            // The daemon state will be updated when the streaming task finishes.
            let mut ds = daemon_state.lock().await;
            if ds.state_machine.state() == State::Recording {
                // Stop the audio capture.
                if let Some(mut capture) = ds.audio_capture.take() {
                    capture.stop();
                    tokio::task::spawn_blocking(move || drop(capture));
                }
                // Transition to transcribing (pipeline is draining).
                if let Err(e) = ds.state_machine.transition(Action::Toggle) {
                    warn!("auto-stop state transition failed: {e}");
                }
            }
            break;
        }

        // Forward to backend.
        if audio_tx.send(chunk).await.is_err() {
            break;
        }
    }

    // Drain remaining audio from the capture channel into the backend.
    while let Some(chunk) = audio_rx.recv().await {
        if audio_tx.send(chunk).await.is_err() {
            break;
        }
    }

    // Close the audio channel to signal end-of-stream to the backend.
    drop(audio_tx);

    // Wait for backend to finish.
    match backend_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let friendly = format_api_error(&e);
            warn!("streaming transcription error: {friendly}");
        }
        Err(e) => {
            warn!("streaming backend task panicked: {e}");
        }
    }

    // Wait for typing to finish.
    let full_text = typing_task.await.unwrap_or_default();

    // If auto-stop happened, we need to transition to Idle.
    let mut ds = daemon_state.lock().await;
    if ds.state_machine.state() == State::Transcribing {
        ds.state_machine.transition(Action::TranscriptionDone).ok();
        if notify {
            let preview = if full_text.len() > 80 {
                format!("{}...", &full_text[..77])
            } else {
                full_text.clone()
            };
            if !preview.is_empty() {
                send_notification("whisrs", &format!("Done: {preview}"));
            }
        }
    }

    Ok(full_text)
}

/// Batch mode: collect all audio, transcribe in one shot, type result.
async fn process_recording_batch(
    capture: Option<AudioCaptureHandle>,
    window_id: Option<&str>,
    context: &DaemonContext,
) -> Result<String> {
    use whisrs::audio::capture::encode_wav;

    let samples = match capture {
        Some(cap) => cap.stop_and_collect().await?,
        None => anyhow::bail!("no audio capture to collect"),
    };

    if samples.is_empty() {
        anyhow::bail!("no audio samples captured");
    }

    info!("collected {} audio samples", samples.len());

    let wav_data = encode_wav(&samples)?;
    info!("encoded WAV: {} bytes", wav_data.len());

    let config = TranscriptionConfig {
        language: context.config.general.language.clone(),
        model: get_model_for_backend(&context.config),
    };

    let text = match context
        .transcription_backend
        .transcribe(&wav_data, &config)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            let friendly = format_api_error(&e);
            error!("transcription failed: {friendly}");
            // Save audio for recovery.
            use whisrs::audio::recovery;
            match recovery::save_recovery_audio(&samples) {
                Ok(path) => {
                    info!("audio saved for recovery: {}", path.display());
                    if context.notify {
                        send_notification(
                            "whisrs",
                            &format!(
                                "Transcription failed: {friendly}\nAudio saved to {}",
                                path.display()
                            ),
                        );
                    }
                }
                Err(re) => {
                    warn!("failed to save recovery audio: {re}");
                }
            }
            return Err(e);
        }
    };

    if text.is_empty() {
        info!("transcription returned empty text — nothing to type");
        return Ok(text);
    }

    // Restore window focus.
    if let Some(wid) = window_id {
        if let Err(e) = context.window_tracker.focus_window(wid) {
            warn!("failed to restore window focus: {e}");
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    // Type the text at the cursor.
    let text_clone = text.clone();
    if let Err(e) = tokio::task::spawn_blocking(move || type_text_at_cursor(&text_clone)).await? {
        warn!("failed to type text: {e}");
    }

    Ok(text)
}

fn format_no_microphone_error() -> String {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let mut msg = "No microphone found — no default audio input device available.".to_string();
    if let Ok(devices) = host.input_devices() {
        let names: Vec<String> = devices.filter_map(|d| d.name().ok()).collect();
        if names.is_empty() {
            msg.push_str("\nNo audio input devices detected. Check that your microphone is connected and PipeWire/PulseAudio is running.");
        } else {
            msg.push_str("\nAvailable input devices:");
            for name in &names {
                msg.push_str(&format!("\n  - {name}"));
            }
            msg.push_str(
                "\nSet the device in ~/.config/whisrs/config.toml under [audio] device = \"...\"",
            );
        }
    }
    msg
}

fn format_api_error(err: &anyhow::Error) -> String {
    let msg = format!("{err}");
    if msg.contains("invalid API key") || msg.contains("401") {
        return "Invalid API key — check your config at ~/.config/whisrs/config.toml".to_string();
    }
    if msg.contains("rate limit") || msg.contains("429") {
        return "Rate limited — wait a moment and try again".to_string();
    }
    if msg.contains("error sending request")
        || msg.contains("dns error")
        || msg.contains("connection refused")
        || msg.contains("timed out")
        || msg.contains("ConnectError")
    {
        return "Cannot reach API — check your internet connection".to_string();
    }
    msg
}

/// Type text at the cursor using uinput (keyboard injection) or clipboard paste.
fn type_text_at_cursor(text: &str) -> Result<()> {
    use whisrs::input::clipboard::ClipboardOps;
    use whisrs::input::keymap::XkbKeymap;
    use whisrs::input::uinput::UinputKeyboard;
    use whisrs::input::KeyInjector;

    let keymap = XkbKeymap::from_default_layout().context("failed to build XKB keymap")?;
    let clipboard = ClipboardOps::detect();
    let mut keyboard = match UinputKeyboard::new(keymap, clipboard) {
        Ok(kb) => kb,
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("Permission denied") || msg.contains("permission") {
                anyhow::bail!(
                    "Cannot open /dev/uinput — permission denied.\n\
                     Fix: sudo usermod -aG input $USER"
                );
            }
            return Err(e.context("failed to create virtual keyboard"));
        }
    };

    keyboard.type_text(text).context("failed to type text")?;
    Ok(())
}

async fn handle_cancel(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) -> Response {
    let mut ds = daemon_state.lock().await;

    match ds.state_machine.transition(Action::Cancel) {
        Ok(new_state) => {
            if let Some(mut capture) = ds.audio_capture.take() {
                capture.stop();
                tokio::task::spawn_blocking(move || drop(capture));
            }
            if let Some(task) = ds.streaming_task.take() {
                task.abort();
            }
            ds.recording_window_id = None;
            info!("cancelled recording");
            if context.notify {
                send_notification("whisrs", "Recording cancelled");
            }
            Response::Ok { state: new_state }
        }
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}
