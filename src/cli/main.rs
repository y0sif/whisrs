use std::process;

use clap::{Parser, Subcommand};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use whisrs::history::HistoryEntry;
use whisrs::{
    encode_message, read_message, restart_daemon_via_systemd, socket_path, Command, Response,
    RestartOutcome, State,
};

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
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

#[derive(Parser)]
#[command(
    name = "whisrs",
    about = "Linux-first voice-to-text dictation tool",
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
    /// Edit any part of ~/.config/whisrs/config.toml; restarts the daemon on save
    Config,
    /// Toggle recording on/off (start dictation or stop and transcribe)
    Toggle {
        /// Override the transcription language for this session: an ISO 639-1
        /// code (e.g. `en`, `pl`), optionally with a region (`en-US`), or `auto`
        #[arg(short, long, value_parser = whisrs::validate_language_override)]
        language: Option<String>,
    },
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
    /// Toggle a named custom LLM command (see [[llm_commands]] in config.toml):
    /// dictate, the LLM applies the configured instruction, result is typed
    /// at the cursor. Press again to stop recording, same as `toggle`.
    #[command(name = "llm-command")]
    LlmCommand {
        /// Name of the [[llm_commands]] entry to run.
        name: String,
    },
    /// Reprogram a named LLM command from the current selection: highlight the
    /// new instruction text, then run this — it's saved to config and applied
    /// live. Pairs with an entry's `set_hotkey`.
    #[command(name = "llm-command-set")]
    LlmCommandSet {
        /// Name of the [[llm_commands]] entry to reprogram.
        name: String,
    },
    /// Read the selected text aloud via TTS (press again to stop playback)
    #[command(alias = "read")]
    Speak,
    /// Restart the whisrs daemon (uses the systemd user service when present)
    Restart,
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
        State::Synthesizing => format!("{BOLD}{CYAN}synthesizing{RESET}"),
        State::Speaking => format!("{BOLD}{GREEN}speaking{RESET}"),
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
        SubCmd::Config => {
            if let Err(e) = whisrs::config::edit::run_config_menu() {
                if is_tty() {
                    eprintln!("{RED}config failed:{RESET} {e:#}");
                } else {
                    eprintln!("config failed: {e:#}");
                }
                process::exit(1);
            }
        }
        SubCmd::Toggle { language } => {
            send_command(Command::Toggle { language }).await?;
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
        SubCmd::LlmCommand { name } => {
            send_command(Command::LlmCommand { name }).await?;
        }
        SubCmd::LlmCommandSet { name } => {
            send_command(Command::SetLlmInstruction { name }).await?;
        }
        SubCmd::Speak => {
            send_command(Command::Speak).await?;
        }
        SubCmd::Restart => {
            cmd_restart()?;
        }
    }

    Ok(())
}

/// Restart the whisrs daemon.
///
/// Uses the systemd user service when `whisrs.service` is loaded; otherwise
/// prints guidance for non-systemd setups. We don't try to `pkill whisrsd`
/// ourselves because that races with respawn and silently breaks for users
/// who launched the daemon under tmux/foot/etc.
fn cmd_restart() -> anyhow::Result<()> {
    let use_color = is_tty();

    if use_color {
        println!("{BOLD}Restarting whisrs daemon (systemd)…{RESET}");
    } else {
        println!("Restarting whisrs daemon (systemd)…");
    }

    match restart_daemon_via_systemd() {
        RestartOutcome::Restarted => {
            if use_color {
                println!("{GREEN}Daemon restarted.{RESET}");
            } else {
                println!("Daemon restarted.");
            }
            Ok(())
        }
        RestartOutcome::Failed => {
            if use_color {
                eprintln!("{RED}systemctl --user restart whisrs.service failed.{RESET}");
            } else {
                eprintln!("systemctl --user restart whisrs.service failed.");
            }
            process::exit(1);
        }
        RestartOutcome::NoSystemdUnit => {
            if use_color {
                eprintln!(
                    "{YELLOW}No whisrs systemd user unit detected.{RESET}\n\
                     \n\
                     Install the systemd unit (run `whisrs setup` and accept the systemd step),\n\
                     or restart the daemon manually:\n\
                     \n\
                     \x20 pkill whisrsd; sleep 0.2; whisrsd &"
                );
            } else {
                eprintln!(
                    "No whisrs systemd user unit detected.\n\
                     \n\
                     Install the systemd unit (run `whisrs setup` and accept the systemd step),\n\
                     or restart the daemon manually:\n\
                     \n\
                     \x20 pkill whisrsd; sleep 0.2; whisrsd &"
                );
            }
            process::exit(1);
        }
    }
}

/// Connect to the daemon and send a command, printing the response.
async fn send_command(cmd: Command) -> anyhow::Result<()> {
    let path = socket_path();
    let use_color = is_tty();

    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(_) => {
            if use_color {
                eprintln!(
                    "{RED}whisrsd is not running.{RESET} Start it with:\n\
                     \n\
                     \x20 whisrsd &\n\
                     \n\
                     Or enable the systemd service:\n\
                     \n\
                     \x20 systemctl --user enable --now whisrs.service"
                );
            } else {
                eprintln!(
                    "whisrsd is not running. Start it with:\n\
                     \n\
                     \x20 whisrsd &\n\
                     \n\
                     Or enable the systemd service:\n\
                     \n\
                     \x20 systemctl --user enable --now whisrs.service"
                );
            }
            process::exit(1);
        }
    };

    let (mut reader, mut writer) = stream.into_split();

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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    /// Issue #84: `llm-command` and `llm-command-set` existed in the clap CLI
    /// (`SubCmd` above) but were never added to the man page's SUBCOMMANDS
    /// section, and nothing caught it. Rather than hand-maintain a mirror
    /// list of subcommand names (which is exactly what went stale), this
    /// walks the real `Cli` derive so a future subcommand that isn't
    /// documented fails automatically.
    #[test]
    fn man_page_documents_all_cli_subcommands() {
        let page_path = format!("{}/contrib/whisrs.1", env!("CARGO_MANIFEST_DIR"));
        let page = std::fs::read_to_string(&page_path)
            .unwrap_or_else(|e| panic!("failed to read {page_path}: {e}"));

        let section = subcommands_section(&page);
        let headers = entry_headers(section);

        let cli = Cli::command();
        for sub in cli.get_subcommands() {
            let name = sub.get_name();
            assert!(
                headers.iter().any(|h| h == name),
                "contrib/whisrs.1: SUBCOMMANDS section is missing an entry header for \
                 `{name}` (expected a `.TP` block headed by `.B {name}` or `\\fB{name}\\fR`); \
                 found headers: {headers:?}"
            );
        }
    }

    /// Slice out the `.SH SUBCOMMANDS` section body, up to (but not
    /// including) the next `.SH `. Man pages repeat subcommand names as
    /// prose elsewhere (DESCRIPTION references `.B setup`, the `speak` entry
    /// mentions `.B cancel`, EXAMPLES shows `whisrs setup`, ...), so a
    /// whole-page search can't tell a real entry from a passing mention.
    /// Scoping to this section is necessary but not sufficient on its own —
    /// see `entry_headers` for the rest.
    fn subcommands_section(page: &str) -> &str {
        const HEADER: &str = ".SH SUBCOMMANDS";
        let start = page
            .find(HEADER)
            .expect("contrib/whisrs.1: missing .SH SUBCOMMANDS section");
        let rest = &page[start + HEADER.len()..];
        let end = rest.find("\n.SH ").unwrap_or(rest.len());
        &rest[..end]
    }

    /// Collect the subcommand names introduced by real `.TP` entry headers
    /// in `section` — i.e. the line immediately following a `.TP` macro,
    /// when that line is itself a `.B`/`.BR` macro or an inline `\fB...\fR`
    /// bold run. This deliberately ignores `.B name` / `\fBname\fR` used in
    /// body text (not directly under `.TP`), which is what let `.B setup`
    /// (referenced from the `config` entry) and `.B cancel` (referenced from
    /// the `speak` entry) mask a deleted header in a plain substring search.
    ///
    /// Escaped groff hyphens (`\-`) are normalized to plain `-` first so
    /// hyphenated subcommand names (`llm-command`, `llm-command-set`) match
    /// regardless of whether the man page escapes them.
    fn entry_headers(section: &str) -> Vec<String> {
        let normalized = section.replace("\\-", "-");
        let lines: Vec<&str> = normalized.lines().collect();

        let mut headers = Vec::new();
        for i in 0..lines.len() {
            if lines[i].trim() != ".TP" {
                continue;
            }
            let Some(header) = lines.get(i + 1).map(|l| l.trim()) else {
                continue;
            };

            if let Some(rest) = header.strip_prefix(".BR ") {
                if let Some(name) = rest.split_whitespace().next() {
                    headers.push(name.to_string());
                }
            } else if let Some(rest) = header.strip_prefix(".B ") {
                headers.push(rest.trim().to_string());
            } else if let Some(rest) = header.strip_prefix("\\fB") {
                if let Some(end) = rest.find("\\fR") {
                    headers.push(rest[..end].to_string());
                }
            }
        }
        headers
    }
}
