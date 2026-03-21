use std::process;

use clap::{Parser, Subcommand};
use tokio::io::AsyncWriteExt;

use whisrs::history::HistoryEntry;
use whisrs::{encode_message, read_message, socket_path, Command, Response, State};

const ASCII_BANNER: &str = concat!(
    "\n",
    "         __    _\n",
    "  _    _| |__ |_|___ _ __ ___\n",
    " \\ \\//\\ / '_ \\| / __| '__/ __|\n",
    "  \\  /\\ \\ | | | \\__ \\ |  \\__ \\\n",
    "   \\/  \\/|_| |_|_|___/_|  |___/\n",
    "\n",
    "  speak. type. done.\n",
    "\n",
    env!("CARGO_PKG_VERSION"),
);

// ANSI color codes.
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

#[derive(Parser)]
#[command(
    name = "whisrs",
    about = "Voice-to-text dictation tool",
    long_version = ASCII_BANNER,
)]
struct Cli {
    #[command(subcommand)]
    command: SubCmd,
}

#[derive(Subcommand)]
enum SubCmd {
    /// Interactive onboarding — pick a backend, set API key, test microphone
    Setup,
    /// Toggle recording on/off (start dictation or stop and transcribe)
    Toggle,
    /// Cancel the current recording and discard audio
    Cancel,
    /// Query the daemon state (idle, recording, transcribing)
    Status,
    /// Show recent transcription history
    Log {
        /// Number of entries to show (default: 20)
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// Clear all history
        #[arg(long)]
        clear: bool,
    },
    /// Command mode: select text, speak an instruction, LLM rewrites it in place
    Command,
}

/// Check if stdout is a TTY for color support.
fn is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Format a state for display with optional color.
fn format_state(state: State, use_color: bool) -> String {
    if !use_color {
        return format!("{state}");
    }

    match state {
        State::Idle => format!("{BOLD}idle{RESET}"),
        State::Recording => format!("{BOLD}{GREEN}recording{RESET}"),
        State::Transcribing => format!("{BOLD}{YELLOW}transcribing{RESET}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        SubCmd::Setup => {
            if let Err(e) = whisrs::config::setup::run_setup() {
                if is_tty() {
                    eprintln!("{RED}setup failed:{RESET} {e:#}");
                } else {
                    eprintln!("setup failed: {e:#}");
                }
                process::exit(1);
            }
        }
        SubCmd::Toggle => {
            send_command(Command::Toggle).await?;
        }
        SubCmd::Cancel => {
            send_command(Command::Cancel).await?;
        }
        SubCmd::Status => {
            send_command(Command::Status).await?;
        }
        SubCmd::Log { limit, clear } => {
            if clear {
                send_command(Command::ClearHistory).await?;
            } else {
                send_command(Command::Log { limit }).await?;
            }
        }
        SubCmd::Command => {
            send_command(Command::CommandMode).await?;
        }
    }

    Ok(())
}

/// Connect to the daemon and send a command, printing the response.
async fn send_command(cmd: Command) -> anyhow::Result<()> {
    let path = socket_path();
    let use_color = is_tty();

    let stream = match connect_to_daemon(&path).await {
        Ok(s) => s,
        Err(_) => {
            print_daemon_not_running(use_color);
            process::exit(1);
        }
    };

    let (mut reader, mut writer) = tokio::io::split(stream);

    // Send command.
    let encoded = encode_message(&cmd)?;
    writer.write_all(&encoded).await?;
    writer.shutdown().await?;

    // Read response.
    let response: Response = read_message(&mut reader).await?;

    match response {
        Response::Ok { state } => {
            println!("{}", format_state(state, use_color));
        }
        Response::History { entries } => {
            if entries.is_empty() {
                println!("No transcription history.");
            } else {
                print_history(&entries, use_color);
            }
        }
        Response::Error { message } => {
            if use_color {
                eprintln!("{RED}error:{RESET} {message}");
            } else {
                eprintln!("error: {message}");
            }
            process::exit(1);
        }
    }

    Ok(())
}

/// Connect to the daemon via platform-appropriate IPC.
#[cfg(unix)]
async fn connect_to_daemon(path: &std::path::Path) -> std::io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(path).await
}

/// Connect to the daemon via named pipe (Windows).
#[cfg(windows)]
async fn connect_to_daemon(
    _path: &std::path::Path,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    ClientOptions::new().open(r"\\.\pipe\whisrs")
}

/// Print a message when the daemon is not running.
fn print_daemon_not_running(use_color: bool) {
    if use_color {
        eprintln!(
            "{RED}whisrsd is not running.{RESET} Start it with:\n\
             \n\
             \x20 whisrsd &"
        );
    } else {
        eprintln!(
            "whisrsd is not running. Start it with:\n\
             \n\
             \x20 whisrsd &"
        );
    }

    #[cfg(target_os = "linux")]
    {
        eprintln!(
            "\nOr enable the systemd service:\n\
             \n\
             \x20 systemctl --user enable --now whisrs.service"
        );
    }
}

/// Display transcription history entries.
fn print_history(entries: &[HistoryEntry], use_color: bool) {
    let dim = if use_color { "\x1b[2m" } else { "" };

    for entry in entries {
        let ts = entry.timestamp.format("%Y-%m-%d %H:%M:%S");
        let duration = format!("{:.1}s", entry.duration_secs);

        if use_color {
            println!(
                "{dim}{ts}{RESET}  {dim}[{backend} | {lang} | {dur}]{RESET}",
                backend = entry.backend,
                lang = entry.language,
                dur = duration,
            );
        } else {
            println!(
                "{ts}  [{backend} | {lang} | {dur}]",
                backend = entry.backend,
                lang = entry.language,
                dur = duration,
            );
        }
        println!("  {}", entry.text);
        println!();
    }
}
