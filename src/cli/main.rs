use std::process;

mod tui;

#[cfg(feature = "recording")]
use clap::ValueEnum;
use clap::{Args, Parser, Subcommand};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use whisrs::config::profile::{self, Check, Paths, Unanswered};
use whisrs::history::HistoryEntry;
#[cfg(feature = "recording")]
use whisrs::recording::{store, RecordingAction, RecordingMeta, RecordingOptions, RecordingState};
use whisrs::recording::{RecordingPhase, RecordingStatus};
use whisrs::{
    encode_message, read_message, service::ServiceManager, socket_path, Command, DaemonInfo,
    InfoAnswer, RequestError, Response, RestartOutcome, State,
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
    /// Interactive onboarding: backend, key, language, microphone, service
    Setup,
    /// Edit any part of ~/.config/whisrs/config.toml; restarts the daemon on save
    Config,
    /// Toggle dictation on/off (start, or stop and transcribe)
    Toggle {
        /// Override the transcription language for this session: an ISO 639-1
        /// code (e.g. `en`, `pl`), optionally with a region (`en-US`), or `auto`
        #[arg(short, long, value_parser = whisrs::validate_language_override)]
        language: Option<String>,
    },
    /// Cancel the current dictation and discard its audio
    Cancel,
    /// Query the dictation state (idle, recording, transcribing), and the
    /// config profile when one is active; for a recording see `whisrs
    /// recording status`
    Status,
    /// List the config profiles (the default), or switch to one: next, base,
    /// or a profile's name (profiles/<name>.toml beside config.toml). A
    /// switch restarts the daemon on it, and is refused while it is busy
    Profile {
        /// list, next, base, or a profile's name
        #[arg(value_name = "list|next|base|NAME")]
        target: Option<String>,
    },
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
    Speak {
        /// Speak this text instead of the selection (`-` reads it from
        /// stdin). Stops a read-aloud in progress first
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        text: Option<String>,
    },
    /// Talk to a coding agent (see [[agents]] in config.toml). With no name,
    /// list the agents. With a name, record a voice instruction for it (same
    /// as its hotkey; press again to stop), or send --text. The reply is
    /// spoken when the turn ends, and the next turn continues the session
    Agent {
        /// The [[agents]] entry
        name: Option<String>,
        /// Send this instruction instead of recording one (`-` reads it from
        /// stdin)
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        text: Option<String>,
        /// Start a fresh session: record a voice instruction for it (press
        /// again, or the agent's hotkey, to stop and send), or send --text
        #[arg(long)]
        new: bool,
        /// Forget the session and send nothing; the next turn starts a new one
        #[arg(long, conflicts_with_all = ["text", "new", "stop"])]
        reset: bool,
        /// Stop the running turn (or the session's own work) and keep the session
        #[arg(long, conflicts_with_all = ["text", "new"])]
        stop: bool,
    },
    /// Forward a coding-agent hook event to the daemon (run by the hooks
    /// whisrs installs in the sessions it starts). Always exits 0, within two
    /// seconds
    #[command(name = "agent-hook", hide = true)]
    AgentHook {
        /// The daemon socket (default: the usual one)
        #[arg(long, value_name = "PATH")]
        socket: Option<std::path::PathBuf>,
    },
    /// Restart the whisrs daemon (uses the systemd or OpenRC user service when present)
    Restart,
    /// Record your mic and the system audio, then turn them into a transcript
    #[cfg(feature = "recording")]
    #[command(subcommand)]
    Recording(RecordingCmd),
    /// Record your mic and the system audio (not in this build: it was built
    /// without the recording feature)
    #[cfg(not(feature = "recording"))]
    Recording(RecordingStub),
    /// Serve your recordings to an AI agent: a read-only MCP server on stdin
    /// and stdout (add it to an MCP client, e.g. `claude mcp add whisrs --
    /// whisrs mcp`)
    Mcp,
}

/// The transcription languages of a recording (`start`, `toggle`, `retry`).
#[cfg(feature = "recording")]
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
struct LanguageArgs {
    /// Language of both tracks: an ISO 639-1 code (e.g. `de`), optionally
    /// with a region (`en-US`), or `auto`
    #[arg(short, long, value_parser = whisrs::validate_language_override)]
    language: Option<String>,
    /// Language of your microphone track, over --language
    #[arg(long, value_name = "LANGUAGE", value_parser = whisrs::validate_language_override)]
    mic_language: Option<String>,
    /// Language of the system audio track, over --language
    #[arg(long, value_name = "LANGUAGE", value_parser = whisrs::validate_language_override)]
    system_language: Option<String>,
}

#[cfg(feature = "recording")]
impl LanguageArgs {
    fn into_options(self, others: Option<String>, speakers: SpeakerArgs) -> RecordingOptions {
        RecordingOptions {
            language: self.language,
            mic_language: self.mic_language,
            system_language: self.system_language,
            others,
            shared_mic: speakers.shared_mic,
            num_speakers: speakers.num_speakers,
            participants: speakers.with.map(|p| p.0),
            ..Default::default()
        }
    }
}

/// What `whisrs recording name` sends: the speaker, and the name's words
/// joined with spaces (an empty one clears it, which the daemon decides).
#[cfg(feature = "recording")]
fn name_options(speaker: String, name: &[String]) -> RecordingOptions {
    RecordingOptions {
        speaker: Some(speaker),
        name: Some(name.join(" ")),
        ..Default::default()
    }
}

/// The names `--with` lists, parsed by
/// [`whisrs::recording::naming::parse_participants`]. A newtype so clap
/// takes the option once, as one value.
#[cfg(feature = "recording")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Participants(Vec<String>);

#[cfg(feature = "recording")]
fn parse_with(list: &str) -> Result<Participants, String> {
    whisrs::recording::naming::parse_participants(list).map(Participants)
}

/// How the voices of a recording are told apart (`start`, `toggle`,
/// `retry`).
#[cfg(feature = "recording")]
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
struct SpeakerArgs {
    /// Several people share your mic (an in-person meeting): tell its voices
    /// apart too. `--shared-mic=false` turns it off for this recording
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
    shared_mic: Option<bool>,
    /// How many voices the diarization sidecar should find on the system
    /// audio (on your mic when there is no system audio); 0 lets it decide
    #[arg(
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u32).range(0..=i64::from(whisrs::recording::NUM_SPEAKERS_MAX))
    )]
    num_speakers: Option<u32>,
    /// Who the recording is with, separated by commas (e.g. "Sarah, Omar"),
    /// to name the voices: one voice and one name, or the last voice left
    /// when the others are named. On `retry` it replaces the stored names;
    /// `--with ""` clears them
    #[arg(long = "with", value_name = "NAMES", value_parser = parse_with)]
    with: Option<Participants>,
}

#[cfg(feature = "recording")]
#[derive(Subcommand)]
enum RecordingCmd {
    /// Start a recording (mic + system audio)
    Start {
        /// Recording title, used in the transcript and the folder name
        #[arg(short, long)]
        title: Option<String>,
        #[command(flatten)]
        languages: LanguageArgs,
        /// Name of the system audio track in the transcript (default: Others)
        #[arg(long, value_name = "NAME", value_parser = whisrs::recording::validate_others_label)]
        others: Option<String>,
        #[command(flatten)]
        speakers: SpeakerArgs,
    },
    /// Stop the recording and queue it for transcription
    Stop,
    /// Start a recording, or stop the one in progress (bind this to a hotkey)
    Toggle {
        /// Recording title when this starts a recording
        #[arg(short, long)]
        title: Option<String>,
        // The languages apply when this starts a recording.
        #[command(flatten)]
        languages: LanguageArgs,
        /// Name of the system audio track when this starts a recording
        /// (default: Others)
        #[arg(long, value_name = "NAME", value_parser = whisrs::recording::validate_others_label)]
        others: Option<String>,
        #[command(flatten)]
        speakers: SpeakerArgs,
    },
    /// Stop the recording and delete it
    Cancel,
    /// Show whether a recording is in progress or processing, and which
    /// recordings wait for the network
    Status,
    /// List recordings, newest first
    List {
        /// Print a JSON array instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Print a recording's transcript
    Show {
        /// Recording id, a unique id prefix, or `last`
        id: String,
        /// Output format
        #[arg(short, long, value_enum, default_value_t = ShowFormat::Md)]
        format: ShowFormat,
    },
    /// Print a recording's folder
    Path {
        /// Recording id, a unique id prefix, or `last`
        id: String,
    },
    /// Copy a recording's transcript to the clipboard
    Copy {
        /// Recording id, a unique id prefix, or `last`
        id: String,
    },
    /// Transcribe a recording again (e.g. after a failure, or in another
    /// language: a language or speaker option given here replaces the
    /// stored one)
    Retry {
        /// Recording id, a unique id prefix, or `last`
        id: String,
        #[command(flatten)]
        languages: LanguageArgs,
        #[command(flatten)]
        speakers: SpeakerArgs,
    },
    /// Name a speaker of a transcribed recording. The transcript is
    /// rewritten at once (nothing is transcribed again), and `retry` keeps
    /// the name
    Name {
        /// Recording id, a unique id prefix, or `last`
        id: String,
        /// The speaker: its id (S2), "Speaker 2", or the name it has now
        speaker: String,
        /// The new name, quoted or not; "" clears the name you gave
        #[arg(required = true, num_args = 1..)]
        name: Vec<String>,
    },
    /// Summarize a transcribed recording with the [llm] now, whatever
    /// [recording] summary says, and title it when you gave it no title.
    /// Nothing is transcribed again
    Summarize {
        /// Recording id, a unique id prefix, or `last`
        id: String,
    },
    /// Delete a recording for good: its audio, transcript and summary. Asks
    /// first on a terminal
    Delete {
        /// Recording id, or a unique id prefix (never `last`)
        id: String,
        /// Delete without asking; needed when not on a terminal
        #[arg(short, long)]
        yes: bool,
    },
}

/// `whisrs recording ...` in a build without the recording feature: it takes
/// any arguments, so every recording command still parses and then says why
/// it cannot run, instead of clap calling it unknown.
#[cfg(not(feature = "recording"))]
#[derive(Args)]
struct RecordingStub {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    _args: Vec<String>,
}

/// Output format of `whisrs recording show`.
#[cfg(feature = "recording")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ShowFormat {
    /// The rendered Markdown transcript (`<id>.md`)
    Md,
    /// The raw `recording.json`
    Json,
    /// Plain text, one line per utterance
    Txt,
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
            let result = tokio::task::spawn_blocking(|| tui::run(tui::Entry::Setup)).await?;
            if let Err(e) = result {
                // Its own whole message, not a failure of the setup.
                if let Some(refusal) = e.downcast_ref::<tui::NotATerminal>() {
                    eprintln!("{refusal}");
                    process::exit(1);
                }
                if is_tty() {
                    eprintln!("{RED}setup failed:{RESET} {e:#}");
                } else {
                    eprintln!("setup failed: {e:#}");
                }
                process::exit(1);
            }
        }
        SubCmd::Config => {
            let result = tokio::task::spawn_blocking(|| tui::run(tui::Entry::Config)).await?;
            if let Err(e) = result {
                // Its own whole message, not a failure of the config.
                if let Some(refusal) = e.downcast_ref::<tui::NotATerminal>() {
                    eprintln!("{refusal}");
                    process::exit(1);
                }
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
        SubCmd::Status => cmd_status().await?,
        SubCmd::Profile { target } => cmd_profile(target).await?,
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
        SubCmd::Speak { text: None } => {
            send_command(Command::Speak).await?;
        }
        SubCmd::Speak { text: Some(text) } => {
            let text = text_arg(text);
            send_command(Command::SpeakText { text }).await?;
        }
        SubCmd::Agent {
            name,
            text,
            new,
            reset,
            stop,
        } => {
            let cmd = match agent_command(name, text.map(text_arg), new, reset, stop) {
                Ok(cmd) => cmd,
                Err(message) => exit_with_error(&message),
            };
            send_command(cmd).await?;
        }
        SubCmd::AgentHook { socket } => cmd_agent_hook(socket).await,
        SubCmd::Restart => {
            cmd_restart()?;
        }
        SubCmd::Recording(cmd) => {
            cmd_recording(cmd).await?;
        }
        SubCmd::Mcp => cmd_mcp().await?,
    }

    Ok(())
}

/// `whisrs mcp`: the MCP server lives in the library ([`whisrs::mcp`]) and
/// reads the recordings folder itself, so it works with the daemon stopped.
#[cfg(feature = "mcp")]
async fn cmd_mcp() -> anyhow::Result<()> {
    whisrs::mcp::serve_stdio(recordings_root()).await
}

/// What `whisrs recording` says in a build without the `recording` feature.
#[cfg(not(feature = "recording"))]
const RECORDING_MISSING: &str = "this whisrs was built without the recording feature, so \
     `whisrs recording` is not available. Use a release build, or rebuild with `cargo install \
     whisrs --features recording`.";

/// `whisrs recording ...` without the feature: the error, exit 1. Nothing
/// is sent to the daemon.
#[cfg(not(feature = "recording"))]
async fn cmd_recording(_cmd: RecordingStub) -> anyhow::Result<()> {
    exit_with_error(RECORDING_MISSING)
}

/// What `whisrs mcp` says in a build without the `mcp` feature.
#[cfg(not(feature = "mcp"))]
const MCP_MISSING: &str = "this whisrs was built without the mcp feature, so `whisrs mcp` is \
     not available. Use a release build, or rebuild with `cargo install whisrs --features mcp`.";

#[cfg(not(feature = "mcp"))]
async fn cmd_mcp() -> anyhow::Result<()> {
    exit_with_error(MCP_MISSING)
}

/// Restart the whisrs daemon.
///
/// Delegates to whichever init system is managing the daemon (systemd or
/// OpenRC), and prints guidance when neither has a whisrs service installed.
/// We don't try to `pkill whisrsd` ourselves because that races with respawn
/// and silently breaks for users who launched the daemon under tmux/foot/etc.
fn cmd_restart() -> anyhow::Result<()> {
    let use_color = is_tty();
    let manager = ServiceManager::detect();

    let banner = format!("Restarting whisrs daemon ({})…", manager.name());
    if use_color {
        println!("{BOLD}{banner}{RESET}");
    } else {
        println!("{banner}");
    }

    match manager.restart() {
        RestartOutcome::Restarted => {
            if use_color {
                println!("{GREEN}Daemon restarted.{RESET}");
            } else {
                println!("Daemon restarted.");
            }
            Ok(())
        }
        RestartOutcome::Failed => {
            let hint = manager.restart_hint().unwrap_or("restart");
            if use_color {
                eprintln!("{RED}{hint} failed.{RESET}");
            } else {
                eprintln!("{hint} failed.");
            }
            process::exit(1);
        }
        RestartOutcome::NoService => {
            let detail = match manager.enable_hint() {
                Some(enable) => format!(
                    "No whisrs service installed for {}.\n\
                     \n\
                     Install it (run `whisrs setup` and accept the service step), then:\n\
                     \n\
                     \x20 {enable}\n\
                     \n\
                     Or restart the daemon manually:\n\
                     \n\
                     \x20 pkill whisrsd; sleep 0.2; whisrsd &",
                    manager.name()
                ),
                None => "No service manager detected (neither systemd nor OpenRC).\n\
                     \n\
                     Restart the daemon manually:\n\
                     \n\
                     \x20 pkill whisrsd; sleep 0.2; whisrsd &"
                    .to_string(),
            };
            if use_color {
                eprintln!("{YELLOW}{detail}{RESET}");
            } else {
                eprintln!("{detail}");
            }
            process::exit(1);
        }
    }
}

/// Connect to the daemon and send a command, printing the response.
async fn send_command(cmd: Command) -> anyhow::Result<()> {
    let use_color = is_tty();
    let response = request(cmd).await?;

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
        Response::Recording { recording } => {
            println!("{}", format_recording_status(&recording, use_color));
        }
        Response::Agents { agents } => print_agents(&agents, use_color),
        Response::Info { info } => match info.state {
            Some(state) => println!("{}", format_state(state, use_color)),
            None => println!("unknown"),
        },
        Response::Profile {
            active,
            switching,
            skipped,
        } => {
            println!("{}", switch_answer(&active, switching, &skipped))
        }
        Response::Error { message } => exit_with_error(&message),
    }

    Ok(())
}

/// `whisrs status`: the state, then the profile line ([`status_lines`]).
/// Asked with `info`; a daemon older than it hangs up (and logs an error for
/// it) and is asked `status`. No timeout, as before.
async fn cmd_status() -> anyhow::Result<()> {
    match tokio::task::spawn_blocking(|| whisrs::request_info_blocking(None)).await? {
        Ok(answer) => {
            for line in status_lines(&answer, is_tty()) {
                println!("{line}");
            }
        }
        Err(RequestError::NotRunning(_)) => exit_not_running(),
        Err(RequestError::Daemon(message)) => exit_with_error(&message),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// What `whisrs status` prints: the state as ever, then, on a terminal only
/// (`tty`, which colours the state too), `profile: <name>` when one other
/// than base is active, or why the active one did not load. Off one the
/// state is the whole output, so a script or status bar that compares it
/// keeps working. An older daemon says nothing of profiles.
fn status_lines(answer: &InfoAnswer, tty: bool) -> Vec<String> {
    let state = match answer.state() {
        Some(state) => format_state(state, tty),
        None => "unknown".to_string(),
    };
    let mut lines = vec![state];
    if !tty {
        return lines;
    }
    if let InfoAnswer::Info(info) = answer {
        if let Some(error) = &info.profile_error {
            lines.push(error.clone());
        } else if let Some(profile) = &info.profile {
            lines.push(format!("profile: {profile}"));
        }
    }
    lines
}

/// How long a profile switch waits for the daemon's answer.
const SWITCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `whisrs profile [list|next|base|<name>]`. `list` (the default) reads the
/// profiles folder and the state file, so it works with the daemon down. A
/// switch goes through the daemon, which checks and restarts; with the
/// daemon down it is checked here and written for its next start.
async fn cmd_profile(target: Option<String>) -> anyhow::Result<()> {
    let paths = Paths::current();
    let target = target.unwrap_or_else(|| "list".to_string());
    if target == "list" {
        let daemon = tokio::task::spawn_blocking(|| {
            whisrs::request_info_blocking(Some(std::time::Duration::from_secs(2)))
        })
        .await?;
        let daemon = match daemon {
            Ok(InfoAnswer::Info(info)) => Some(info),
            _ => None,
        };
        let (lines, warnings) = match profile_list(&paths, daemon.as_ref()) {
            Ok(listed) => listed,
            Err(why) => exit_with_error(&why),
        };
        for warning in warnings {
            if is_tty() {
                eprintln!("{YELLOW}warning:{RESET} {warning}");
            } else {
                eprintln!("warning: {warning}");
            }
        }
        for line in lines {
            println!("{line}");
        }
        return Ok(());
    }

    let name = (target != "next").then_some(target);
    let switched =
        tokio::task::spawn_blocking(move || switch_profile(&paths, &socket_path(), name)).await?;
    match switched {
        Ok(line) => println!("{line}"),
        Err(why) => exit_with_error(&why),
    }
    Ok(())
}

/// A switch from the CLI: sent to the daemon at `socket`. One that does not
/// answer is down, and the switch is checked and written for its next start
/// ([`switch_offline`]), unless it looks to be restarting
/// ([`profile::unanswered`]): then it is waited for and sent the switch, or
/// still restarting, refused. The line to print, or the refusal.
fn switch_profile(
    paths: &Paths,
    socket: &std::path::Path,
    name: Option<String>,
) -> Result<String, String> {
    let cmd = Command::SwitchProfile { name: name.clone() };
    let send = || whisrs::request_blocking_at(socket, &cmd, Some(SWITCH_TIMEOUT));
    let mut answer = send();
    if matches!(answer, Err(RequestError::NotRunning(_))) {
        match profile::unanswered(socket, &paths.state) {
            Unanswered::Down => return switch_offline(paths, name.as_deref()),
            Unanswered::Back => answer = send(),
            Unanswered::Restarting => return Err(profile::RESTARTING.to_string()),
        }
    }
    match answer {
        Ok(Response::Profile {
            active,
            switching,
            skipped,
        }) => Ok(switch_answer(&active, switching, &skipped)),
        Ok(Response::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected daemon response: {other:?}")),
        // Back a moment ago, gone again.
        Err(RequestError::NotRunning(_)) => Err(profile::RESTARTING.to_string()),
        Err(RequestError::Closed) if older_daemon_cannot_parse(&cmd) => Err(
            "whisrsd closed the connection without answering. It may be older than this \
             whisrs and not know this command: restart it (whisrs restart)."
                .to_string(),
        ),
        Err(e) => Err(e.to_string()),
    }
}

/// The daemon's answer to a switch, as `whisrs profile` prints it, with
/// what `next` skipped.
fn switch_answer(active: &str, switching: bool, skipped: &[String]) -> String {
    if switching {
        profile::switching_line(active, skipped)
    } else {
        format!("already on {active}")
    }
}

/// A switch with the daemon down: checked as a client checks it (a key that
/// may be in the daemon's environment alone is no failure), then written to
/// the state file for its next start. The error is the refusal.
fn switch_offline(paths: &Paths, asked: Option<&str>) -> Result<String, String> {
    let listing = profile::list(&paths.profiles)
        .map_err(|e| format!("cannot read {}: {e}", paths.profiles.display()))?;
    let (current, readable) = match profile::read_active(&paths.state) {
        Ok(current) => (current, true),
        Err(_) => (None, false),
    };
    // `next` passes over what would not load, as the daemon's does.
    let (target, skipped) = match asked.map(str::trim) {
        None | Some("next") => {
            profile::next_usable(paths, &listing.names, current.as_deref(), Check::Client)?
        }
        asked => (
            profile::resolve_target(&listing.names, current.as_deref(), asked)?,
            Vec::new(),
        ),
    };
    let shown = target.as_deref().unwrap_or(profile::BASE);
    if readable && target == current {
        return Ok(format!("already on {shown} (whisrsd is not running)"));
    }
    profile::switch_check(paths, target.as_deref(), Check::Client)?;
    profile::set_active(&paths.state, target.as_deref())
        .map_err(|e| format!("cannot write {}: {e}", paths.state.display()))?;
    Ok(format!(
        "whisrsd is not running; {shown} applies when it starts{}",
        profile::skipped_note(&skipped)
    ))
}

/// What `whisrs profile list` prints: one line per profile, base first, the
/// active one (as the state file names it) marked `*`, a broken one or one
/// setting base-only keys noted; above them the daemon's line when it runs
/// on another. Beside them the warnings: files skipped, a state file that
/// names no profile, or a profile with no file.
fn profile_list(
    paths: &Paths,
    daemon: Option<&DaemonInfo>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let listing = profile::list(&paths.profiles)
        .map_err(|e| format!("cannot read {}: {e}", paths.profiles.display()))?;
    let mut warnings: Vec<String> = listing.skipped.iter().map(|s| s.warning()).collect();
    let active = match profile::read_active(&paths.state) {
        Ok(active) => active,
        Err(why) => {
            warnings.push(why);
            None
        }
    };
    if let Some(name) = active
        .as_deref()
        .filter(|a| !listing.names.iter().any(|n| n == a))
    {
        warnings.push(format!(
            "the active profile {name} has no file ({}): whisrsd starts on base",
            profile::label(name)
        ));
    }

    let mut lines = Vec::new();
    if let Some(info) = daemon {
        if let Some(error) = &info.profile_error {
            lines.push(error.clone());
        } else if info.profile != active {
            lines.push(format!(
                "whisrsd runs on {}; {} applies when it restarts",
                info.profile.as_deref().unwrap_or(profile::BASE),
                active.as_deref().unwrap_or(profile::BASE)
            ));
        }
    }
    let width = listing
        .names
        .iter()
        .map(String::len)
        .chain([profile::BASE.len()])
        .max()
        .unwrap_or_default();
    let row = |name: &str, note: Option<String>| {
        let marker = if active.as_deref().unwrap_or(profile::BASE) == name {
            '*'
        } else {
            ' '
        };
        match note {
            Some(note) => format!("{marker} {name:<width$}  ({note})"),
            None => format!("{marker} {name}"),
        }
    };
    lines.push(row(profile::BASE, None));
    let mut base_error = None;
    for name in &listing.names {
        let note = match profile::effective_at_checked(paths, name, Check::Client) {
            Ok(eff) => match eff.error {
                Some(why) => Some(format!("broken: {why}")),
                None if !eff.dropped.is_empty() => {
                    Some(format!("base only, ignored: {}", eff.dropped.join(", ")))
                }
                None => None,
            },
            Err(why) => {
                base_error.get_or_insert(why);
                None
            }
        };
        lines.push(row(name, note));
    }
    if let Some(why) = base_error {
        warnings.push(format!("the profiles were not checked: {why}"));
    }
    if listing.names.is_empty() {
        lines.push(format!(
            "no profiles yet: add one as {}",
            paths.profiles.join("<name>.toml").display()
        ));
    }
    Ok((lines, warnings))
}

/// A `--text` value: `-` reads stdin.
fn text_arg(text: String) -> String {
    if text != "-" {
        return text;
    }
    use std::io::Read;
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        exit_with_error(&format!("cannot read the text from stdin: {e}"));
    }
    // The newline `echo` ends it with is not part of the text.
    buf.trim_end().to_string()
}

/// The IPC command for `whisrs agent [<name>] [--text T] [--new] [--reset]
/// [--stop]`.
fn agent_command(
    name: Option<String>,
    text: Option<String>,
    new: bool,
    reset: bool,
    stop: bool,
) -> Result<Command, String> {
    use whisrs::agent::AgentAction;
    let Some(name) = name else {
        if text.is_some() || new || reset || stop {
            return Err("which agent? Pass its name: whisrs agent <name> ...".to_string());
        }
        return Ok(Command::Agent {
            action: AgentAction::List,
            name: None,
            text: None,
            new: false,
        });
    };
    let (action, new) = match (&text, stop, reset) {
        (_, true, _) => (AgentAction::Stop, false),
        (_, _, true) => (AgentAction::Reset, false),
        (Some(_), false, false) => (AgentAction::Send, new),
        (None, false, false) => (AgentAction::Toggle, new),
    };
    Ok(Command::Agent {
        action,
        name: Some(name),
        text,
        new,
    })
}

/// `whisrs agent` rows: name, harness, state, and the session's background
/// id (what `claude attach`, `claude logs` and `claude stop` take). With no
/// bg id (an OpenCode, Codex or Copilot session, or an older daemon's row)
/// the session id is shown: all of it for those (what `opencode --session`,
/// `codex resume` and `copilot --resume=` take), its first eight characters
/// from an older daemon.
fn print_agents(agents: &[whisrs::agent::AgentInfo], use_color: bool) {
    use whisrs::agent::AgentPhase;
    if agents.is_empty() {
        println!("No agents configured. Add an [[agents]] entry to config.toml.");
        return;
    }
    let name_width = agents
        .iter()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let harness_width = agents
        .iter()
        .map(|a| a.harness.len())
        .max()
        .unwrap_or(7)
        .max(7);
    for agent in agents {
        let phase = agent.phase.to_string();
        let phase = if use_color {
            match agent.phase {
                AgentPhase::Idle => format!("{BOLD}{phase:<9}{RESET}"),
                AgentPhase::Working => format!("{BOLD}{YELLOW}{phase:<9}{RESET}"),
                AgentPhase::NeedsYou => format!("{BOLD}{RED}{phase:<9}{RESET}"),
            }
        } else {
            format!("{phase:<9}")
        };
        let id = agent
            .bg_id
            .as_deref()
            .or(agent.session.as_deref())
            .unwrap_or("-");
        println!(
            "{:<name_width$}  {:<harness_width$}  {phase}  {id}",
            agent.name, agent.harness
        );
    }
}

/// How long `whisrs agent-hook` may take in all.
const AGENT_HOOK_BUDGET: std::time::Duration = std::time::Duration::from_millis(1800);

/// `whisrs agent-hook`: read the hook JSON on stdin and forward it to the
/// daemon. Never blocks the harness: it prints nothing and exits 0 within
/// [`AGENT_HOOK_BUDGET`], also when the daemon is down or the input is bad.
async fn cmd_agent_hook(socket: Option<std::path::PathBuf>) -> ! {
    let _ = tokio::time::timeout(AGENT_HOOK_BUDGET, forward_agent_hook(socket)).await;
    process::exit(0);
}

async fn forward_agent_hook(socket: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    tokio::io::stdin()
        .take(1024 * 1024)
        .read_to_end(&mut buf)
        .await?;
    let payload: serde_json::Value = serde_json::from_slice(&buf)?;
    let path = socket.unwrap_or_else(socket_path);
    let stream = UnixStream::connect(&path).await?;
    let (mut reader, mut writer) = stream.into_split();
    writer
        .write_all(&encode_message(&Command::AgentEvent { payload })?)
        .await?;
    writer.shutdown().await?;
    let _: Response = read_message(&mut reader).await?;
    Ok(())
}

/// Print `error: <message>` to stderr and exit 1.
fn exit_with_error(message: &str) -> ! {
    if is_tty() {
        eprintln!("{RED}error:{RESET} {message}");
    } else {
        eprintln!("error: {message}");
    }
    process::exit(1);
}

/// Print how to start the daemon and exit 1.
fn exit_not_running() -> ! {
    // Name the command that actually exists on this machine: telling
    // an OpenRC user to run systemctl is a dead end.
    let service_hint = match ServiceManager::detect().enable_hint() {
        Some(enable) => format!(
            "\n\
             \n\
             Or enable the service:\n\
             \n\
             \x20 {enable}"
        ),
        None => String::new(),
    };
    let msg = format!(
        "whisrsd is not running. Start it with:\n\
         \n\
         \x20 whisrsd &{service_hint}"
    );
    if is_tty() {
        eprintln!("{RED}{msg}{RESET}");
    } else {
        eprintln!("{msg}");
    }
    process::exit(1);
}

/// Connect to the daemon, send a command and return its response.
/// Exits with a hint when the daemon is not running.
async fn request(cmd: Command) -> anyhow::Result<Response> {
    let Ok(stream) = UnixStream::connect(socket_path()).await else {
        exit_not_running();
    };

    let (mut reader, mut writer) = stream.into_split();

    // Send command.
    let new_command = older_daemon_cannot_parse(&cmd);
    let encoded = encode_message(&cmd)?;
    writer.write_all(&encoded).await?;
    writer.shutdown().await?;

    // Read response. A daemon that cannot parse the command (one older than
    // this CLI) closes the connection without answering; only a command an
    // older daemon may not know gets that hint.
    match read_message::<Response>(&mut reader).await {
        Ok(response) => Ok(response),
        Err(e) if new_command => exit_with_error(&format!(
            "whisrsd closed the connection without answering ({e}). It may be older than this \
             whisrs and not know this command: restart it (whisrs restart)."
        )),
        Err(e) => Err(e),
    }
}

/// Whether a daemon older than this whisrs may not parse `cmd`: `speak-text`,
/// `agent` and `agent-event` are new with agent mode, `info`, `recording
/// delete` and `switch-profile` after it.
fn older_daemon_cannot_parse(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::SpeakText { .. }
            | Command::Agent { .. }
            | Command::AgentEvent { .. }
            | Command::Info
            | Command::Recording {
                action: whisrs::recording::RecordingAction::Delete,
                ..
            }
            | Command::SwitchProfile { .. }
    )
}

/// `whisrs recording <action>`. Recording control goes through the daemon;
/// `list`, `show` and `path` only read the recordings folder, so they work
/// with the daemon stopped.
#[cfg(feature = "recording")]
async fn cmd_recording(cmd: RecordingCmd) -> anyhow::Result<()> {
    let use_color = is_tty();
    let none = RecordingOptions::default;
    let (action, title, id, options) = match cmd {
        RecordingCmd::Start {
            title,
            languages,
            others,
            speakers,
        } => (
            RecordingAction::Start,
            title,
            None,
            languages.into_options(others, speakers),
        ),
        RecordingCmd::Stop => (RecordingAction::Stop, None, None, none()),
        RecordingCmd::Toggle {
            title,
            languages,
            others,
            speakers,
        } => (
            RecordingAction::Toggle,
            title,
            None,
            languages.into_options(others, speakers),
        ),
        RecordingCmd::Cancel => (RecordingAction::Cancel, None, None, none()),
        RecordingCmd::Status => (RecordingAction::Status, None, None, none()),
        RecordingCmd::Copy { id } => (RecordingAction::Copy, None, Some(id), none()),
        RecordingCmd::Retry {
            id,
            languages,
            speakers,
        } => (
            RecordingAction::Retry,
            None,
            Some(id),
            languages.into_options(None, speakers),
        ),
        RecordingCmd::Name { id, speaker, name } => (
            RecordingAction::Name,
            None,
            Some(id),
            name_options(speaker, &name),
        ),
        RecordingCmd::Summarize { id } => (RecordingAction::Summarize, None, Some(id), none()),
        RecordingCmd::Delete { id, yes } => return recording_delete(&id, yes).await,
        RecordingCmd::List { json } => return recording_list(json),
        RecordingCmd::Show { id, format } => return recording_show(&id, format),
        RecordingCmd::Path { id } => {
            let dir = resolve_or_exit(&id);
            println!("{}", dir.display());
            return Ok(());
        }
    };

    let query = id.clone();
    let response = request(Command::Recording {
        action,
        title,
        id,
        options,
    })
    .await?;
    match response {
        Response::Recording { .. } if action == RecordingAction::Name => {
            print_speakers(query.as_deref().unwrap_or("last"));
        }
        Response::Recording { recording } => {
            match action {
                RecordingAction::Copy => println!("Transcript copied to the clipboard."),
                RecordingAction::Retry => println!("Queued for transcription."),
                RecordingAction::Summarize => println!("Queued for a summary."),
                _ => {}
            }
            println!("{}", format_recording_status(&recording, use_color));
        }
        Response::Error { message } => exit_with_error(&message),
        other => exit_with_error(&format!("unexpected daemon response: {other:?}")),
    }
    Ok(())
}

/// `whisrs recording delete`: resolved here (a unique prefix works, `last`
/// never does), asked on a terminal (`--yes` skips it, and is needed off
/// one), then deleted by the daemon by its full id, which refuses it while
/// it is busy.
#[cfg(feature = "recording")]
async fn recording_delete(query: &str, yes: bool) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if let Some(refusal) = delete_refusal(query) {
        exit_with_error(&refusal);
    }
    let (dir, meta) = match store::resolve_meta(&recordings_root(), query) {
        Ok(found) => found,
        Err(e) => exit_with_error(&format!("{e:#}")),
    };
    let question = delete_question_for(&dir, &meta);
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let stdin = std::io::stdin();
    match confirm_delete(
        yes,
        tty,
        &question,
        &meta.id,
        stdin.lock(),
        std::io::stdout(),
    ) {
        Ok(true) => {}
        Ok(false) => {
            println!("Kept {}.", meta.id);
            return Ok(());
        }
        Err(refusal) => exit_with_error(&refusal),
    }
    let response = request(Command::Recording {
        action: RecordingAction::Delete,
        title: None,
        id: Some(meta.id.clone()),
        options: RecordingOptions::default(),
    })
    .await?;
    match response {
        Response::Recording { .. } => println!("Deleted {} ({}).", meta.id, dir.display()),
        Response::Error { message } => exit_with_error(&message),
        other => exit_with_error(&format!("unexpected daemon response: {other:?}")),
    }
    Ok(())
}

/// Why `whisrs recording delete <query>` is refused before anything is
/// read: `last` is never what it deletes.
#[cfg(feature = "recording")]
fn delete_refusal(query: &str) -> Option<String> {
    query.trim().eq_ignore_ascii_case("last").then(|| {
        "delete takes the recording's id, not \"last\", so the newest recording is never \
         deleted by mistake: whisrs recording list shows the ids"
            .to_string()
    })
}

/// The question a delete asks about the recording in `dir`, naming what of
/// it is there to lose.
#[cfg(feature = "recording")]
fn delete_question_for(dir: &std::path::Path, meta: &RecordingMeta) -> String {
    let audio = !meta.audio_deleted && meta.tracks.iter().any(|t| dir.join(&t.file).exists());
    let transcript = store::transcript_path(dir, &meta.id).exists();
    let duration = recording_duration_secs(meta).map(tui::duration_label);
    tui::delete_question(
        meta.title.as_deref().filter(|t| !t.trim().is_empty()),
        &meta.id,
        duration.as_deref(),
        tui::Holds {
            audio,
            transcript,
            summary: meta.summary.is_some(),
        },
    )
}

/// Ask `question` on `output` and read the answer from `input`: only `y` or
/// `yes` deletes. Off a terminal it asks nothing and refuses, unless `yes`
/// was given, which deletes without asking.
#[cfg(feature = "recording")]
fn confirm_delete(
    yes: bool,
    tty: bool,
    question: &str,
    id: &str,
    mut input: impl std::io::BufRead,
    mut output: impl std::io::Write,
) -> Result<bool, String> {
    if yes {
        return Ok(true);
    }
    if !tty {
        return Err(format!(
            "whisrs recording delete asks before it deletes, and this is not a terminal: \
             whisrs recording delete {id} --yes deletes it without asking"
        ));
    }
    let _ = write!(output, "{question} [y/N] ");
    let _ = output.flush();
    let mut answer = String::new();
    if input.read_line(&mut answer).is_err() {
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// The recordings root: `[recording] dir` from config.toml, else the default
/// ([`store::configured_recordings_dir`], which reads only that key).
#[cfg(feature = "recording")]
fn recordings_root() -> std::path::PathBuf {
    store::configured_recordings_dir()
}

/// Resolve `<id|last>` to a recording folder, or print the error and exit 1.
#[cfg(feature = "recording")]
fn resolve_or_exit(query: &str) -> std::path::PathBuf {
    match store::resolve(&recordings_root(), query) {
        Ok(dir) => dir,
        Err(e) => exit_with_error(&format!("{e:#}")),
    }
}

/// `whisrs recording list`.
#[cfg(feature = "recording")]
fn recording_list(json: bool) -> anyhow::Result<()> {
    let root = recordings_root();
    let recordings = match store::list(&root) {
        Ok(m) => m,
        Err(e) => exit_with_error(&format!("{e:#}")),
    };

    if json {
        let items: Vec<serde_json::Value> = recordings
            .iter()
            .map(|(dir, meta)| recording_list_json(dir, meta))
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if recordings.is_empty() {
        println!("No recordings in {}.", root.display());
        return Ok(());
    }
    let id_width = recordings
        .iter()
        .map(|(_, m)| m.id.len())
        .max()
        .unwrap_or(0)
        .max("ID".len());
    println!(
        "{:<id_width$}  {:<16}  {:>8}  {:<10}  TITLE",
        "ID", "STARTED", "DURATION", "STATUS"
    );
    for (_, meta) in &recordings {
        println!("{}", format_recording_row(meta, id_width));
    }
    Ok(())
}

/// One row of the `recording list` table.
#[cfg(feature = "recording")]
fn format_recording_row(meta: &RecordingMeta, id_width: usize) -> String {
    let duration = recording_duration_secs(meta)
        .map(format_duration)
        .unwrap_or_else(|| "-".to_string());
    format!(
        "{:<id_width$}  {:<16}  {:>8}  {:<10}  {}",
        meta.id,
        meta.started_at.format("%Y-%m-%d %H:%M"),
        duration,
        recording_state_name(meta.status),
        meta.title.as_deref().unwrap_or(""),
    )
}

/// One element of `recording list --json`: the metadata without segments.
#[cfg(feature = "recording")]
fn recording_list_json(dir: &std::path::Path, meta: &RecordingMeta) -> serde_json::Value {
    serde_json::json!({
        "id": meta.id,
        "title": meta.title,
        "status": meta.status,
        "started_at": meta.started_at,
        "ended_at": meta.ended_at,
        "duration_secs": recording_duration_secs(meta),
        "dir": dir,
        "error": meta.error,
    })
}

/// Recorded length in whole seconds, or `None` while still recording.
#[cfg(feature = "recording")]
fn recording_duration_secs(meta: &RecordingMeta) -> Option<i64> {
    let ended = meta.ended_at?;
    Some((ended - meta.started_at).num_seconds().max(0))
}

/// `3725` -> `1:02:05`, `125` -> `2:05`.
#[cfg(feature = "recording")]
fn format_duration(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(feature = "recording")]
fn recording_state_name(state: RecordingState) -> &'static str {
    match state {
        RecordingState::Recording => "recording",
        RecordingState::Processing => "processing",
        RecordingState::Done => "done",
        RecordingState::Failed => "failed",
        RecordingState::Waiting => "waiting",
    }
}

/// After `whisrs recording name`: the recording's speakers as the daemon
/// just wrote them, one per line with its id and how it got its name.
#[cfg(feature = "recording")]
fn print_speakers(query: &str) {
    let dir = resolve_or_exit(query);
    let meta = match store::read_meta(&dir) {
        Ok(meta) => meta,
        Err(e) => exit_with_error(&format!("{e:#}")),
    };
    println!("{}", format_speakers(&meta));
}

/// `Speakers of <id>:` and a line per speaker: `  S2  Sarah  (user)`.
#[cfg(feature = "recording")]
fn format_speakers(meta: &RecordingMeta) -> String {
    let width = meta.speakers.iter().map(|s| s.id.len()).max().unwrap_or(0);
    let mut out = format!("Speakers of {}:", meta.id);
    for speaker in &meta.speakers {
        out.push_str(&format!(
            "\n  {:width$}  {}  ({})",
            speaker.id, speaker.name, speaker.source
        ));
    }
    out
}

/// `whisrs recording show`.
#[cfg(feature = "recording")]
fn recording_show(query: &str, format: ShowFormat) -> anyhow::Result<()> {
    let dir = resolve_or_exit(query);
    match format {
        ShowFormat::Json => {
            let path = dir.join("recording.json");
            match std::fs::read_to_string(&path) {
                Ok(text) => print!("{text}"),
                Err(e) => exit_with_error(&format!("failed to read {}: {e}", path.display())),
            }
        }
        ShowFormat::Md | ShowFormat::Txt => {
            let meta = match store::read_meta(&dir) {
                Ok(meta) => meta,
                Err(e) => exit_with_error(&format!("{e:#}")),
            };
            if format == ShowFormat::Txt {
                if meta.status != RecordingState::Done {
                    exit_with_error(&not_ready_message(&meta));
                }
                print!("{}", whisrs::recording::render::render_text(&meta));
            } else {
                let path = store::transcript_path(&dir, &meta.id);
                match std::fs::read_to_string(&path) {
                    Ok(text) => print!("{text}"),
                    Err(e)
                        if e.kind() == std::io::ErrorKind::NotFound
                            && meta.status != RecordingState::Done =>
                    {
                        exit_with_error(&not_ready_message(&meta))
                    }
                    Err(e) => exit_with_error(&format!("failed to read {}: {e}", path.display())),
                }
            }
        }
    }
    Ok(())
}

/// Why `show` has no transcript to print for a recording that is not done.
#[cfg(feature = "recording")]
fn not_ready_message(meta: &RecordingMeta) -> String {
    match meta.status {
        RecordingState::Failed => format!(
            "recording {} failed to process: {}. Run `whisrs recording retry {}` to try again.",
            meta.id,
            meta.error.as_deref().unwrap_or("unknown error"),
            meta.id
        ),
        RecordingState::Waiting => {
            let until = meta
                .network_deadline()
                .map(|d| format!(" until {}", d.format("%Y-%m-%d %H:%M")))
                .unwrap_or_default();
            format!(
                "recording {} is waiting for the network: {}. whisrs tries again every {} \
                 minutes{until}; `whisrs recording retry {}` tries now.",
                meta.id,
                meta.error.as_deref().unwrap_or("no network"),
                whisrs::recording::NETWORK_RETRY_EVERY.as_secs() / 60,
                meta.id
            )
        }
        state => format!(
            "recording {} has no transcript yet (status: {}).",
            meta.id,
            recording_state_name(state)
        ),
    }
}

/// One-line recording status, e.g.
/// `Recording since 14:05 (Weekly sync)  id 2026-10-02_1405_weekly-sync`.
fn format_recording_status(status: &RecordingStatus, use_color: bool) -> String {
    let (word, color) = match status.phase {
        RecordingPhase::Idle => ("Idle", ""),
        RecordingPhase::Recording => ("Recording", GREEN),
        RecordingPhase::Processing if status.summarizing => ("Summarizing", YELLOW),
        RecordingPhase::Processing => ("Processing", YELLOW),
    };
    let mut line = if use_color {
        format!("{BOLD}{color}{word}{RESET}")
    } else {
        word.to_string()
    };
    if status.phase == RecordingPhase::Recording {
        if let Some(started) = status.started_at {
            line.push_str(&format!(" since {}", started.format("%H:%M")));
        }
    }
    if status.phase != RecordingPhase::Idle {
        if let Some(title) = status.title.as_deref().filter(|t| !t.is_empty()) {
            line.push_str(&format!(" ({title})"));
        }
        if let Some(id) = &status.id {
            line.push_str(&format!("  id {id}"));
        }
    }
    if !status.queued.is_empty() {
        line.push_str(&format!("  ({} queued)", status.queued.len()));
    }
    if !status.waiting.is_empty() {
        line.push_str(&format!(
            "  ({} waiting for the network: {})",
            status.waiting.len(),
            status.waiting.join(", ")
        ));
    }
    line
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
    use clap::{CommandFactory, Parser};

    use super::*;

    /// `whisrs mcp` exists in every build, so a build without the feature
    /// says so instead of an unknown subcommand.
    #[test]
    fn mcp_is_a_subcommand_in_every_build() {
        let cli = Cli::try_parse_from(["whisrs", "mcp"]).unwrap();
        assert!(matches!(cli.command, SubCmd::Mcp));
        assert!(Cli::try_parse_from(["whisrs", "mcp", "extra"]).is_err());
    }

    /// Only the commands an older daemon cannot parse get the "older
    /// daemon" hint when the daemon hangs up; every other one keeps its
    /// plain error.
    #[test]
    fn only_new_commands_get_the_older_daemon_hint() {
        use whisrs::agent::AgentAction;
        assert!(older_daemon_cannot_parse(&Command::SpeakText {
            text: "hi".into()
        }));
        assert!(older_daemon_cannot_parse(&Command::Agent {
            action: AgentAction::List,
            name: None,
            text: None,
            new: false,
        }));
        assert!(older_daemon_cannot_parse(&Command::AgentEvent {
            payload: serde_json::json!({}),
        }));
        assert!(older_daemon_cannot_parse(&Command::Info));
        assert!(older_daemon_cannot_parse(&Command::SwitchProfile {
            name: None
        }));
        let recording = |action| Command::Recording {
            action,
            title: None,
            id: Some("2026-10-27_1402_standup".into()),
            options: Default::default(),
        };
        assert!(older_daemon_cannot_parse(&recording(
            whisrs::recording::RecordingAction::Delete
        )));
        for old in [
            recording(whisrs::recording::RecordingAction::Copy),
            recording(whisrs::recording::RecordingAction::Retry),
            Command::Toggle { language: None },
            Command::Cancel,
            Command::Status,
            Command::Speak,
            Command::CommandMode,
            Command::Log { limit: 5 },
        ] {
            assert!(!older_daemon_cannot_parse(&old), "{}", old.log_summary());
        }
    }

    #[test]
    fn profile_is_a_subcommand() {
        let cli = Cli::try_parse_from(["whisrs", "profile"]).unwrap();
        assert!(matches!(cli.command, SubCmd::Profile { target: None }));
        for word in ["list", "next", "base", "offline"] {
            let cli = Cli::try_parse_from(["whisrs", "profile", word]).unwrap();
            assert!(
                matches!(&cli.command, SubCmd::Profile { target: Some(t) } if t == word),
                "{word}"
            );
        }
        assert!(Cli::try_parse_from(["whisrs", "profile", "a", "b"]).is_err());
    }

    /// A config.toml that validates in any environment (the sidecar needs
    /// no key), `profiles` beside it, and `active` in the state file.
    fn profile_dir(profiles: &[(&str, &str)], active: Option<&str>) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::write(
            &paths.config,
            "[general]\nbackend = \"asr-sidecar\"\n[asr-sidecar]\nurl = \"http://127.0.0.1:1\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(&paths.profiles).unwrap();
        for (name, contents) in profiles {
            std::fs::write(paths.profile(name), contents).unwrap();
        }
        if let Some(active) = active {
            profile::set_active(&paths.state, Some(active)).unwrap();
        }
        (dir, paths)
    }

    #[test]
    fn profile_list_marks_the_active_one_and_notes_the_rest() {
        let (_dir, paths) = profile_dir(
            &[
                ("offline", "[general]\nlanguage = \"de\"\n"),
                ("work", "[hotkeys]\ntoggle = \"Super+W\"\n"),
                ("broken", "[general\n"),
            ],
            Some("offline"),
        );
        std::fs::write(paths.profiles.join("notes.txt"), "").unwrap();
        let (lines, warnings) = profile_list(&paths, None).unwrap();
        assert_eq!(
            lines,
            [
                "  base",
                "  broken   (broken: profiles/broken.toml is not valid TOML (line 1, column 9))",
                "* offline",
                "  work     (base only, ignored: hotkeys)",
            ]
        );
        assert_eq!(warnings, ["skipped profiles/notes.txt: not a .toml file"]);

        // The daemon's line, when it runs on another than the state file.
        let on = |profile: Option<&str>, error: Option<&str>| DaemonInfo {
            profile: profile.map(String::from),
            profile_error: error.map(String::from),
            ..Default::default()
        };
        let (lines, _) = profile_list(&paths, Some(&on(None, None))).unwrap();
        assert_eq!(
            lines[0],
            "whisrsd runs on base; offline applies when it restarts"
        );
        let (lines, _) = profile_list(&paths, Some(&on(Some("offline"), None))).unwrap();
        assert_eq!(lines[0], "  base");
        let why = "profile offline did not load, using base: x";
        let (lines, _) = profile_list(&paths, Some(&on(None, Some(why)))).unwrap();
        assert_eq!(lines[0], why);
    }

    #[test]
    fn profile_list_without_profiles_says_how_to_add_one() {
        let (_dir, paths) = profile_dir(&[], None);
        let (lines, warnings) = profile_list(&paths, None).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], "* base");
        assert!(
            lines[1].starts_with("no profiles yet: add one as "),
            "{}",
            lines[1]
        );
        assert!(lines[1].ends_with("profiles/<name>.toml"), "{}", lines[1]);
        assert!(warnings.is_empty());

        // The state file names one whose file is gone.
        profile::set_active(&paths.state, Some("gone")).unwrap();
        let (lines, warnings) = profile_list(&paths, None).unwrap();
        assert_eq!(lines[0], "  base");
        assert_eq!(
            warnings,
            ["the active profile gone has no file (profiles/gone.toml): whisrsd starts on base"]
        );
    }

    /// With the daemon down a switch is checked as the daemon checks it,
    /// then written for its next start.
    #[test]
    fn a_switch_with_the_daemon_down_writes_the_state_file() {
        let (_dir, paths) = profile_dir(
            &[
                ("offline", "[general]\nlanguage = \"de\"\n"),
                ("broken", "[general]\nbackend = \"nope\"\n"),
            ],
            None,
        );
        assert_eq!(
            switch_offline(&paths, Some("offline")).as_deref(),
            Ok("whisrsd is not running; offline applies when it starts")
        );
        assert_eq!(
            profile::read_active(&paths.state),
            Ok(Some("offline".into()))
        );
        assert_eq!(
            switch_offline(&paths, Some("offline")).as_deref(),
            Ok("already on offline (whisrsd is not running)")
        );
        let refusal = switch_offline(&paths, Some("broken")).unwrap_err();
        assert!(
            refusal.starts_with("cannot switch to broken: "),
            "{refusal}"
        );
        assert!(switch_offline(&paths, Some("gone")).is_err());
        assert_eq!(
            profile::read_active(&paths.state),
            Ok(Some("offline".into()))
        );

        // next: broken, offline (sorted), then base.
        assert_eq!(
            switch_offline(&paths, None).as_deref(),
            Ok("whisrsd is not running; base applies when it starts")
        );
        assert_eq!(profile::read_active(&paths.state), Ok(None));
        // From base it passes over broken, which does not load.
        assert_eq!(
            switch_offline(&paths, None).as_deref(),
            Ok(
                "whisrsd is not running; offline applies when it starts (skipped broken: it does \
                not load)"
            )
        );
    }

    #[test]
    fn status_shows_the_profile_on_a_terminal_only() {
        let info = |profile: Option<&str>, error: Option<&str>| {
            InfoAnswer::Info(DaemonInfo {
                state: Some(State::Idle),
                profile: profile.map(String::from),
                profile_error: error.map(String::from),
                ..Default::default()
            })
        };
        let why = "profile offline did not load, using base: profiles/offline.toml does not exist";
        for (profile, error, second) in [
            (Some("offline"), None, Some("profile: offline")),
            (None, Some(why), Some(why)),
            (None, None, None),
        ] {
            let answer = info(profile, error);
            // Off a terminal the output is the state alone, as ever.
            assert_eq!(status_lines(&answer, false), ["idle"]);
            let lines = status_lines(&answer, true);
            assert_eq!(lines[0], format_state(State::Idle, true));
            assert_eq!(lines.get(1).map(String::as_str), second);
            assert!(lines.len() <= 2, "{lines:?}");
        }
    }

    /// A daemon at `path` that binds after `delay`, then answers each
    /// connection with the next of `answers`. Joins to the commands it saw.
    fn fake_daemon_at(
        path: std::path::PathBuf,
        delay: std::time::Duration,
        answers: Vec<Response>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        use std::io::{Read, Write};

        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let mut seen = Vec::new();
            for answer in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut len = [0u8; 4];
                stream.read_exact(&mut len).unwrap();
                let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                stream.read_exact(&mut body).unwrap();
                let cmd: serde_json::Value = serde_json::from_slice(&body).unwrap();
                seen.push(cmd["cmd"].as_str().unwrap_or_default().to_string());
                stream.write_all(&encode_message(&answer).unwrap()).unwrap();
            }
            seen
        })
    }

    /// A daemon that does not answer just after the active name changed is
    /// restarting: it is waited for and gets the switch, and the client
    /// writes nothing. One that is simply down gets the switch written for
    /// its next start.
    #[test]
    fn a_switch_during_a_restart_waits_for_the_daemon() {
        let (dir, paths) = profile_dir(&[("offline", "")], Some("work"));
        let socket = dir.path().join("whisrs.sock");
        let daemon = fake_daemon_at(
            socket.clone(),
            std::time::Duration::from_millis(300),
            vec![
                Response::Ok { state: State::Idle },
                Response::Profile {
                    active: "offline".into(),
                    switching: true,
                    skipped: vec!["broken".into()],
                },
            ],
        );
        assert_eq!(
            switch_profile(&paths, &socket, Some("offline".into())).as_deref(),
            Ok("switching to offline (skipped broken: it does not load)")
        );
        assert_eq!(daemon.join().unwrap(), ["status", "switch-profile"]);
        assert_eq!(profile::read_active(&paths.state), Ok(Some("work".into())));

        let (dir, paths) = profile_dir(&[("offline", "")], None);
        let socket = dir.path().join("whisrs.sock");
        assert_eq!(
            switch_profile(&paths, &socket, Some("offline".into())).as_deref(),
            Ok("whisrsd is not running; offline applies when it starts")
        );
    }

    /// With the daemon down, a profile whose backend key is in no config
    /// and not in this shell is no failure: the daemon's environment may
    /// hold it, and its own dry run decides.
    #[test]
    fn a_key_only_the_daemon_may_have_does_not_refuse_an_offline_switch() {
        if std::env::var_os("WHISRS_GROQ_API_KEY").is_some() {
            return;
        }
        let (_dir, paths) = profile_dir(&[("cloud", "[general]\nbackend = \"groq\"\n")], None);
        assert!(
            profile::dry_run(&paths, "cloud").is_err(),
            "premise: the daemon refuses"
        );
        let (lines, _) = profile_list(&paths, None).unwrap();
        assert_eq!(lines[1], "  cloud");
        assert_eq!(
            switch_offline(&paths, Some("cloud")).as_deref(),
            Ok("whisrsd is not running; cloud applies when it starts")
        );
    }

    /// With the daemon down a switch to base is refused too while
    /// config.toml does not load.
    #[test]
    fn an_offline_switch_to_base_is_refused_on_a_broken_config_toml() {
        let (_dir, paths) = profile_dir(&[("offline", "")], Some("offline"));
        std::fs::write(&paths.config, "[general\n").unwrap();
        assert_eq!(
            switch_offline(&paths, Some("base")),
            Err("cannot switch to base: config.toml is not valid TOML (line 1, column 9)".into())
        );
        assert_eq!(
            profile::read_active(&paths.state),
            Ok(Some("offline".into()))
        );
    }

    /// `whisrs status` against a daemon older than `info`: it hangs up on
    /// `info`, is asked `status`, and the state is the whole answer.
    #[test]
    fn status_falls_back_to_status_on_an_older_daemon() {
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("whisrs.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let daemon = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for answer in [
                None,
                Some(Response::Ok {
                    state: State::Transcribing,
                }),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut len = [0u8; 4];
                stream.read_exact(&mut len).unwrap();
                let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                stream.read_exact(&mut body).unwrap();
                let cmd: serde_json::Value = serde_json::from_slice(&body).unwrap();
                seen.push(cmd["cmd"].as_str().unwrap_or_default().to_string());
                if let Some(answer) = answer {
                    stream.write_all(&encode_message(&answer).unwrap()).unwrap();
                }
            }
            seen
        });
        let answer =
            whisrs::request_info_blocking_at(&path, Some(std::time::Duration::from_secs(5)))
                .unwrap();
        assert_eq!(status_lines(&answer, false), ["transcribing"]);
        assert_eq!(daemon.join().unwrap(), ["info", "status"]);
    }

    #[cfg(not(feature = "mcp"))]
    #[test]
    fn mcp_without_the_feature_says_how_to_get_it() {
        assert!(MCP_MISSING.contains("built without the mcp feature"));
        assert!(MCP_MISSING.contains("--features mcp"));
    }

    fn at(h: u32, m: u32) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(2026, 10, 2, h, m, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn recording_status_idle() {
        assert_eq!(
            format_recording_status(&RecordingStatus::default(), false),
            "Idle"
        );
    }

    #[test]
    fn recording_status_in_progress() {
        let status = RecordingStatus {
            phase: RecordingPhase::Recording,
            id: Some("2026-10-02_1405_weekly-sync".into()),
            title: Some("Weekly sync".into()),
            started_at: Some(at(14, 5)),
            ..Default::default()
        };
        assert_eq!(
            format_recording_status(&status, false),
            "Recording since 14:05 (Weekly sync)  id 2026-10-02_1405_weekly-sync"
        );

        let untitled = RecordingStatus {
            title: None,
            id: Some("2026-10-02_1405".into()),
            ..status
        };
        assert_eq!(
            format_recording_status(&untitled, false),
            "Recording since 14:05  id 2026-10-02_1405"
        );
    }

    #[test]
    fn recording_status_processing_with_queue() {
        let status = RecordingStatus {
            phase: RecordingPhase::Processing,
            id: Some("a".into()),
            started_at: Some(at(9, 0)),
            queued: vec!["b".into(), "c".into()],
            ..Default::default()
        };
        assert_eq!(
            format_recording_status(&status, false),
            "Processing  id a  (2 queued)"
        );
        let summarizing = RecordingStatus {
            summarizing: true,
            queued: Vec::new(),
            ..status
        };
        assert_eq!(
            format_recording_status(&summarizing, false),
            "Summarizing  id a"
        );
    }

    #[test]
    fn recording_status_color_wraps_phase_only() {
        let status = RecordingStatus {
            phase: RecordingPhase::Recording,
            started_at: Some(at(8, 30)),
            ..Default::default()
        };
        assert_eq!(
            format_recording_status(&status, true),
            format!("{BOLD}{GREEN}Recording{RESET} since 08:30")
        );
    }

    #[test]
    fn recording_status_lists_the_ones_waiting_for_the_network() {
        let status = RecordingStatus {
            waiting: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        assert_eq!(
            format_recording_status(&status, false),
            "Idle  (2 waiting for the network: a, b)"
        );
    }

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
        // A hidden subcommand (`agent-hook`, run by harness hooks) is not for
        // users, so it is neither in `--help` nor in the man page.
        for sub in cli.get_subcommands().filter(|s| !s.is_hide_set()) {
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

    /// Built without the recording feature, every recording command still
    /// parses, so it reaches the error below instead of clap's "unrecognized
    /// subcommand", and nothing in it is checked.
    #[cfg(not(feature = "recording"))]
    #[test]
    fn recording_parses_in_a_build_without_the_feature() {
        for args in [
            &["recording"][..],
            &[
                "recording",
                "start",
                "-t",
                "Weekly sync",
                "--with",
                "Sarah, Omar",
            ],
            &["recording", "toggle", "--num-speakers", "99"],
            &["recording", "show", "last", "-f", "txt"],
            &["recording", "delete", "2026-10", "--yes"],
            &["recording", "bogus", "--flag"],
        ] {
            let argv: Vec<&str> = std::iter::once("whisrs")
                .chain(args.iter().copied())
                .collect();
            let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert!(matches!(cli.command, SubCmd::Recording(_)), "{args:?}");
        }
    }

    #[cfg(not(feature = "recording"))]
    #[test]
    fn recording_without_the_feature_says_how_to_get_it() {
        assert_eq!(
            RECORDING_MISSING,
            "this whisrs was built without the recording feature, so `whisrs recording` is \
             not available. Use a release build, or rebuild with `cargo install whisrs \
             --features recording`."
        );
        assert!(!RECORDING_MISSING.contains('\u{2014}'), "no em dash");
    }

    /// The recording commands, in a build with the feature.
    #[cfg(feature = "recording")]
    mod recording {
        use super::*;

        fn parse_recording(args: &[&str]) -> RecordingCmd {
            let mut argv = vec!["whisrs", "recording"];
            argv.extend_from_slice(args);
            match Cli::try_parse_from(argv).expect("parse").command {
                SubCmd::Recording(cmd) => cmd,
                _ => panic!("not a recording command"),
            }
        }

        #[test]
        fn recording_start_parses_title() {
            assert!(matches!(
                parse_recording(&["start"]),
                RecordingCmd::Start { title: None, languages, others: None, speakers }
                    if languages == LanguageArgs::default() && speakers == SpeakerArgs::default()
            ));
            for flag in ["-t", "--title"] {
                match parse_recording(&["start", flag, "Weekly sync"]) {
                    RecordingCmd::Start { title, .. } => {
                        assert_eq!(title.as_deref(), Some("Weekly sync"))
                    }
                    _ => panic!("expected start"),
                }
            }
        }

        #[test]
        fn recording_toggle_parses_title() {
            assert!(matches!(
                parse_recording(&["toggle"]),
                RecordingCmd::Toggle { title: None, languages, others: None, speakers }
                    if languages == LanguageArgs::default() && speakers == SpeakerArgs::default()
            ));
            match parse_recording(&["toggle", "-t", "Weekly sync"]) {
                RecordingCmd::Toggle { title, .. } => {
                    assert_eq!(title.as_deref(), Some("Weekly sync"))
                }
                _ => panic!("expected toggle"),
            }
        }

        /// The language practice example from the docs, on `start` and `toggle`.
        #[test]
        fn recording_start_and_toggle_parse_languages_and_others() {
            let want = RecordingOptions {
                language: None,
                mic_language: Some("de".into()),
                system_language: Some("auto".into()),
                others: Some("Video".into()),
                shared_mic: None,
                num_speakers: None,
                participants: None,
                ..Default::default()
            };
            for action in ["start", "toggle"] {
                let cmd = parse_recording(&[
                    action,
                    "-t",
                    "Lesson 50",
                    "--mic-language",
                    "DE",
                    "--system-language",
                    "auto",
                    "--others",
                    " Video ",
                ]);
                let options = match cmd {
                    RecordingCmd::Start {
                        languages,
                        others,
                        speakers,
                        ..
                    }
                    | RecordingCmd::Toggle {
                        languages,
                        others,
                        speakers,
                        ..
                    } => languages.into_options(others, speakers),
                    _ => panic!("expected {action}"),
                };
                assert_eq!(options, want, "{action}: normalized as the daemon would");
            }
            for flag in ["-l", "--language"] {
                match parse_recording(&["start", flag, "de"]) {
                    RecordingCmd::Start { languages, .. } => {
                        assert_eq!(languages.language.as_deref(), Some("de"))
                    }
                    _ => panic!("expected start"),
                }
            }
            for bad in [
                &["start", "-l", "english"][..],
                &["toggle", "--mic-language", "1"],
                &["start", "--system-language", ""],
                &["start", "--others", "  "],
            ] {
                let mut argv = vec!["whisrs", "recording"];
                argv.extend_from_slice(bad);
                assert!(
                    Cli::try_parse_from(argv).is_err(),
                    "{bad:?} must be refused"
                );
            }
        }

        /// `retry` takes the three languages, and no title or others label.
        #[test]
        fn recording_retry_parses_languages() {
            match parse_recording(&["retry", "last", "-l", "de", "--system-language", "auto"]) {
                RecordingCmd::Retry {
                    id,
                    languages,
                    speakers,
                } => {
                    assert_eq!(id, "last");
                    assert_eq!(
                        languages.into_options(None, speakers),
                        RecordingOptions {
                            language: Some("de".into()),
                            system_language: Some("auto".into()),
                            ..Default::default()
                        }
                    );
                }
                _ => panic!("expected retry"),
            }
            for extra in ["--others", "--title"] {
                let argv = ["whisrs", "recording", "retry", "last", extra, "x"];
                assert!(Cli::try_parse_from(argv).is_err(), "retry takes no {extra}");
            }
        }

        /// `--shared-mic` sets it, `--shared-mic=false` clears it for one
        /// recording, and leaving it out keeps the default (or what a retried
        /// recording stored), on `start`, `toggle` and `retry` alike.
        #[test]
        fn recording_shared_mic_parses() {
            let shared = |args: &[&str]| match parse_recording(args) {
                RecordingCmd::Start { speakers, .. }
                | RecordingCmd::Toggle { speakers, .. }
                | RecordingCmd::Retry { speakers, .. } => speakers.shared_mic,
                _ => panic!("expected start, toggle or retry"),
            };
            for head in [&["start"][..], &["toggle"], &["retry", "last"]] {
                let with = |extra: &[&str]| {
                    let mut args = head.to_vec();
                    args.extend_from_slice(extra);
                    shared(&args)
                };
                assert_eq!(with(&[]), None, "{head:?}");
                assert_eq!(with(&["--shared-mic"]), Some(true), "{head:?}");
                assert_eq!(with(&["--shared-mic=true"]), Some(true), "{head:?}");
                assert_eq!(with(&["--shared-mic=false"]), Some(false), "{head:?}");
            }
            let argv = ["whisrs", "recording", "start", "--shared-mic=maybe"];
            assert!(Cli::try_parse_from(argv).is_err());
            // The option does not take the next argument as its value.
            match parse_recording(&["retry", "--shared-mic", "last"]) {
                RecordingCmd::Retry { id, speakers, .. } => {
                    assert_eq!(id, "last");
                    assert_eq!(speakers.shared_mic, Some(true));
                }
                _ => panic!("expected retry"),
            }
        }

        /// `--num-speakers` takes 0 (let the sidecar decide) to 20 on `start`,
        /// `toggle` and `retry`, and reaches the options sent to the daemon.
        #[test]
        fn recording_num_speakers_parses() {
            for head in [&["start"][..], &["toggle"], &["retry", "last"]] {
                for (value, want) in [("0", 0), ("3", 3), ("20", 20)] {
                    let mut args = head.to_vec();
                    args.extend_from_slice(&["--num-speakers", value]);
                    let speakers = match parse_recording(&args) {
                        RecordingCmd::Start { speakers, .. }
                        | RecordingCmd::Toggle { speakers, .. }
                        | RecordingCmd::Retry { speakers, .. } => speakers,
                        _ => panic!("expected start, toggle or retry"),
                    };
                    let options = LanguageArgs::default().into_options(None, speakers);
                    assert_eq!(options.num_speakers, Some(want), "{args:?}");
                }
                for bad in ["21", "-1", "two"] {
                    let mut argv = vec!["whisrs", "recording"];
                    argv.extend_from_slice(head);
                    argv.extend_from_slice(&["--num-speakers", bad]);
                    assert!(Cli::try_parse_from(argv).is_err(), "{head:?} {bad}");
                }
            }
        }

        /// `--with` takes names separated by commas on `start`, `toggle` and
        /// `retry`, trimmed; a blank list is nobody; bad lists are refused
        /// before anything reaches the daemon.
        #[test]
        fn recording_with_parses() {
            for head in [&["start"][..], &["toggle"], &["retry", "last"]] {
                let options = |extra: &[&str]| {
                    let mut args = head.to_vec();
                    args.extend_from_slice(extra);
                    match parse_recording(&args) {
                        RecordingCmd::Start { speakers, .. }
                        | RecordingCmd::Toggle { speakers, .. }
                        | RecordingCmd::Retry { speakers, .. } => {
                            LanguageArgs::default().into_options(None, speakers)
                        }
                        _ => panic!("expected start, toggle or retry"),
                    }
                };
                assert_eq!(options(&[]).participants, None, "{head:?}");
                assert_eq!(
                    options(&["--with", " Sarah, Omar Haddad "]).participants,
                    Some(vec!["Sarah".to_string(), "Omar Haddad".to_string()]),
                    "{head:?}"
                );
                assert_eq!(
                    options(&["--with", ""]).participants,
                    Some(Vec::new()),
                    "{head:?}"
                );
                let too_many = (0..21).map(|i| format!("p{i}")).collect::<Vec<_>>();
                for bad in [
                    "Sarah,,Omar",
                    "Sarah, sarah",
                    "Sarah\u{7}",
                    &too_many.join(","),
                ] {
                    let mut argv = vec!["whisrs", "recording"];
                    argv.extend_from_slice(head);
                    argv.extend_from_slice(&["--with", bad]);
                    assert!(Cli::try_parse_from(argv).is_err(), "{head:?} {bad}");
                }
            }
        }

        /// `name` takes the recording, the speaker and the name, which may be
        /// several words; an empty name clears it, and none at all is refused.
        #[test]
        fn recording_name_parses() {
            let options = |args: &[&str]| match parse_recording(args) {
                RecordingCmd::Name { id, speaker, name } => {
                    let options = name_options(speaker, &name);
                    (id, options.speaker.unwrap(), options.name.unwrap())
                }
                _ => panic!("expected name"),
            };
            assert_eq!(
                options(&["name", "last", "S2", "Sarah"]),
                ("last".into(), "S2".into(), "Sarah".into())
            );
            assert_eq!(
                options(&["name", "2026-10", "Speaker 2", "Omar", "Haddad"]),
                ("2026-10".into(), "Speaker 2".into(), "Omar Haddad".into())
            );
            assert_eq!(options(&["name", "last", "S2", ""]).2, "");
            for bad in [&["name", "last", "S2"][..], &["name", "last"]] {
                let mut argv = vec!["whisrs", "recording"];
                argv.extend_from_slice(bad);
                assert!(Cli::try_parse_from(argv).is_err(), "{bad:?}");
            }
        }

        #[test]
        fn speakers_print_with_their_ids_and_sources() {
            let mut meta = RecordingMeta::new("m1".into(), None, chrono::Local::now());
            let speaker = |id: &str, name: &str, source: &str| whisrs::recording::Speaker {
                id: id.into(),
                name: name.into(),
                source: source.into(),
                evidence: None,
                talk_ms: 0,
                channel: None,
                minor: false,
            };
            meta.speakers = vec![
                speaker("me", "Yosif", "self"),
                speaker("S1", "Sarah", "user"),
                speaker("S12", "Speaker 1", "none"),
            ];
            assert_eq!(
                format_speakers(&meta),
                "Speakers of m1:\n  me   Yosif  (self)\n  S1   Sarah  (user)\n  S12  Speaker 1  (none)"
            );
        }

        #[test]
        fn recording_plain_actions_parse() {
            assert!(matches!(parse_recording(&["stop"]), RecordingCmd::Stop));
            assert!(matches!(parse_recording(&["cancel"]), RecordingCmd::Cancel));
            assert!(matches!(parse_recording(&["status"]), RecordingCmd::Status));
            assert!(matches!(
                parse_recording(&["list"]),
                RecordingCmd::List { json: false }
            ));
            assert!(matches!(
                parse_recording(&["list", "--json"]),
                RecordingCmd::List { json: true }
            ));
        }

        #[test]
        fn recording_id_actions_parse() {
            match parse_recording(&["path", "last"]) {
                RecordingCmd::Path { id } => assert_eq!(id, "last"),
                _ => panic!("expected path"),
            }
            match parse_recording(&["copy", "2026-10-02"]) {
                RecordingCmd::Copy { id } => assert_eq!(id, "2026-10-02"),
                _ => panic!("expected copy"),
            }
            match parse_recording(&["retry", "last"]) {
                RecordingCmd::Retry {
                    id,
                    languages,
                    speakers,
                } => {
                    assert_eq!(id, "last");
                    assert_eq!(languages, LanguageArgs::default());
                    assert_eq!(speakers, SpeakerArgs::default());
                }
                _ => panic!("expected retry"),
            }
            match parse_recording(&["summarize", "last"]) {
                RecordingCmd::Summarize { id } => assert_eq!(id, "last"),
                _ => panic!("expected summarize"),
            }
            for action in ["show", "path", "copy", "retry", "summarize"] {
                let argv = ["whisrs", "recording", action];
                assert!(Cli::try_parse_from(argv).is_err(), "{action} needs an id");
            }
        }

        /// `delete` takes the id and `--yes` (`-y`), and needs the id.
        #[test]
        fn recording_delete_parses() {
            for (args, yes) in [
                (&["delete", "2026-10"][..], false),
                (&["delete", "2026-10", "--yes"], true),
                (&["delete", "-y", "2026-10"], true),
            ] {
                match parse_recording(args) {
                    RecordingCmd::Delete { id, yes: given } => {
                        assert_eq!((id.as_str(), given), ("2026-10", yes), "{args:?}")
                    }
                    _ => panic!("expected delete"),
                }
            }
            assert!(Cli::try_parse_from(["whisrs", "recording", "delete"]).is_err());
        }

        /// `last` is never what a delete takes, in any case; a prefix is
        /// left to the lookup.
        #[test]
        fn a_delete_never_takes_last() {
            for last in ["last", " LAST "] {
                let refusal = delete_refusal(last).expect("refused");
                assert!(refusal.contains("not \"last\""), "{refusal}");
                assert!(refusal.contains("whisrs recording list"), "{refusal}");
            }
            assert_eq!(delete_refusal("2026-10-27"), None);
        }

        /// Off a terminal a delete asks nothing and refuses unless `--yes`,
        /// which deletes without asking; on one it asks `[y/N]` and only
        /// `y` or `yes` deletes.
        #[test]
        fn a_delete_asks_on_a_terminal_and_refuses_off_one() {
            let question = "Delete \"Standup\" (32m): audio. This cannot be undone.";
            let id = "2026-10-27_1402_standup";
            let mut out = Vec::new();
            let refusal =
                confirm_delete(false, false, question, id, &b"y\n"[..], &mut out).unwrap_err();
            assert!(refusal.contains("this is not a terminal"), "{refusal}");
            assert!(
                refusal.contains("whisrs recording delete 2026-10-27_1402_standup --yes"),
                "{refusal}"
            );
            assert!(out.is_empty(), "nothing asked");
            assert_eq!(
                confirm_delete(true, false, question, id, &b""[..], &mut out),
                Ok(true)
            );
            assert_eq!(
                confirm_delete(true, true, question, id, &b"n\n"[..], &mut out),
                Ok(true),
                "--yes does not ask"
            );
            assert!(out.is_empty());
            for (answer, delete) in [
                ("y\n", true),
                (" YES \n", true),
                ("\n", false),
                ("n\n", false),
                ("yeah\n", false),
                ("", false),
            ] {
                let mut out = Vec::new();
                assert_eq!(
                    confirm_delete(false, true, question, id, answer.as_bytes(), &mut out),
                    Ok(delete),
                    "{answer:?}"
                );
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    format!("{question} [y/N] ")
                );
            }
        }

        /// The question names the recording and only what of it is on disk.
        #[test]
        fn the_delete_question_names_what_is_on_disk() {
            let tmp = tempfile::tempdir().unwrap();
            let (id, dir) = store::create(tmp.path(), at(14, 2), Some("Standup")).unwrap();
            let mut meta = RecordingMeta::new(id.clone(), Some("Standup".into()), at(14, 2));
            meta.status = RecordingState::Done;
            meta.ended_at = Some(at(14, 34));
            meta.tracks = vec![whisrs::recording::TrackMeta {
                kind: whisrs::recording::TrackKind::Mic,
                file: "audio/mic.wav".into(),
                device: "default".into(),
                offset_ms: 0,
                samples: 0,
                quality: None,
            }];
            meta.summary = Some("Shipped.".into());
            std::fs::write(store::transcript_path(&dir, &id), "# Standup\n").unwrap();
            assert_eq!(
                delete_question_for(&dir, &meta),
                "Delete \"Standup\" (32m): transcript and summary. This cannot be undone."
            );
            std::fs::write(dir.join("audio/mic.wav"), b"RIFF").unwrap();
            assert_eq!(
                delete_question_for(&dir, &meta),
                "Delete \"Standup\" (32m): audio, transcript and summary. This cannot be undone."
            );
            meta.audio_deleted = true;
            meta.title = None;
            meta.summary = None;
            assert_eq!(
                delete_question_for(&dir, &meta),
                format!("Delete {id} (32m): transcript. This cannot be undone.")
            );
        }

        #[test]
        fn recording_show_format_defaults_to_md() {
            match parse_recording(&["show", "last"]) {
                RecordingCmd::Show { id, format } => {
                    assert_eq!(id, "last");
                    assert_eq!(format, ShowFormat::Md);
                }
                _ => panic!("expected show"),
            }
            for (arg, want) in [
                ("md", ShowFormat::Md),
                ("json", ShowFormat::Json),
                ("txt", ShowFormat::Txt),
            ] {
                match parse_recording(&["show", "last", "--format", arg]) {
                    RecordingCmd::Show { format, .. } => assert_eq!(format, want),
                    _ => panic!("expected show"),
                }
            }
            match parse_recording(&["show", "x", "-f", "json"]) {
                RecordingCmd::Show { format, .. } => assert_eq!(format, ShowFormat::Json),
                _ => panic!("expected show"),
            }
            assert!(
                Cli::try_parse_from(["whisrs", "recording", "show", "x", "--format", "pdf"])
                    .is_err()
            );
        }

        #[test]
        fn recording_requires_an_action() {
            assert!(Cli::try_parse_from(["whisrs", "recording"]).is_err());
            assert!(Cli::try_parse_from(["whisrs", "recording", "bogus"]).is_err());
        }

        #[test]
        fn duration_formatting() {
            assert_eq!(format_duration(0), "0:00");
            assert_eq!(format_duration(125), "2:05");
            assert_eq!(format_duration(3725), "1:02:05");
        }

        #[test]
        fn recording_row_and_json() {
            let mut meta = RecordingMeta::new(
                "2026-10-02_1405_sync".into(),
                Some("Sync".into()),
                at(14, 5),
            );
            assert_eq!(recording_duration_secs(&meta), None);
            assert!(format_recording_row(&meta, 20).contains("       -  recording   Sync"));

            meta.ended_at = Some(at(15, 7));
            meta.status = RecordingState::Done;
            assert_eq!(recording_duration_secs(&meta), Some(62 * 60));
            assert_eq!(
                format_recording_row(&meta, 20),
                "2026-10-02_1405_sync  2026-10-02 14:05   1:02:00  done        Sync"
            );

            let json = recording_list_json(std::path::Path::new("/m/x"), &meta);
            assert_eq!(json["id"], "2026-10-02_1405_sync");
            assert_eq!(json["status"], "done");
            assert_eq!(json["duration_secs"], 3720);
            assert_eq!(json["dir"], "/m/x");
            assert!(json.get("segments").is_none());
        }

        #[test]
        fn not_ready_message_names_status_and_retry() {
            let mut meta = RecordingMeta::new("m1".into(), None, at(10, 0));
            assert_eq!(
                not_ready_message(&meta),
                "recording m1 has no transcript yet (status: recording)."
            );
            meta.status = RecordingState::Failed;
            meta.error = Some("backend down".into());
            assert!(not_ready_message(&meta).contains("backend down"));
            assert!(not_ready_message(&meta).contains("whisrs recording retry m1"));

            meta.status = RecordingState::Waiting;
            meta.ended_at = Some(at(11, 0));
            meta.error = Some("could not reach the transcription service".into());
            // The window runs from the first failed try (11:01), not the stop.
            meta.network_wait = Some(whisrs::recording::NetworkWait {
                since: at(11, 1),
                attempts: 0,
                last_attempt_at: at(11, 1),
            });
            assert_eq!(
                not_ready_message(&meta),
                "recording m1 is waiting for the network: could not reach the transcription \
                 service. whisrs tries again every 10 minutes until 2026-10-03 11:01; \
                 `whisrs recording retry m1` tries now."
            );
            assert!(format_recording_row(&meta, 2).contains("  waiting     "));
            let json = recording_list_json(std::path::Path::new("/m/m1"), &meta);
            assert_eq!(json["status"], "waiting");
            assert_eq!(json["error"], "could not reach the transcription service");
        }
    }

    #[test]
    fn speak_takes_an_optional_text() {
        let cli = Cli::try_parse_from(["whisrs", "speak"]).unwrap();
        assert!(matches!(cli.command, SubCmd::Speak { text: None }));
        let cli = Cli::try_parse_from(["whisrs", "read"]).unwrap();
        assert!(matches!(cli.command, SubCmd::Speak { text: None }));
        let cli = Cli::try_parse_from(["whisrs", "speak", "--text", "hello there"]).unwrap();
        assert!(matches!(cli.command, SubCmd::Speak { text: Some(t) } if t == "hello there"));
        // `-` is stdin, not a flag.
        let cli = Cli::try_parse_from(["whisrs", "speak", "--text", "-"]).unwrap();
        assert!(matches!(cli.command, SubCmd::Speak { text: Some(t) } if t == "-"));
    }

    fn parse_agent(args: &[&str]) -> Result<Command, String> {
        let mut argv = vec!["whisrs", "agent"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv).map_err(|e| e.to_string())?;
        let SubCmd::Agent {
            name,
            text,
            new,
            reset,
            stop,
        } = cli.command
        else {
            panic!("not the agent subcommand");
        };
        agent_command(name, text, new, reset, stop)
    }

    #[test]
    fn agent_subcommand_maps_to_its_actions() {
        use whisrs::agent::AgentAction;
        let action = |args: &[&str]| match parse_agent(args).unwrap() {
            Command::Agent {
                action,
                name,
                text,
                new,
            } => (action, name, text, new),
            other => panic!("{other:?}"),
        };
        assert_eq!(action(&[]), (AgentAction::List, None, None, false));
        assert_eq!(
            action(&["jarvis"]),
            (AgentAction::Toggle, Some("jarvis".into()), None, false)
        );
        assert_eq!(
            action(&["jarvis", "--text", "run the tests"]),
            (
                AgentAction::Send,
                Some("jarvis".into()),
                Some("run the tests".into()),
                false
            )
        );
        assert_eq!(
            action(&["jarvis", "--new", "--text", "fresh start"]),
            (
                AgentAction::Send,
                Some("jarvis".into()),
                Some("fresh start".into()),
                true
            )
        );
        // --new alone is a voice turn in a fresh session.
        assert_eq!(
            action(&["jarvis", "--new"]),
            (AgentAction::Toggle, Some("jarvis".into()), None, true)
        );
        assert_eq!(
            action(&["jarvis", "--reset"]),
            (AgentAction::Reset, Some("jarvis".into()), None, false)
        );
        assert_eq!(
            action(&["jarvis", "--stop"]),
            (AgentAction::Stop, Some("jarvis".into()), None, false)
        );
    }

    #[test]
    fn agent_subcommand_rejects_contradictions() {
        assert!(parse_agent(&["jarvis", "--stop", "--new"]).is_err());
        assert!(parse_agent(&["jarvis", "--stop", "--text", "x"]).is_err());
        for other in [&["--new"][..], &["--stop"], &["--text", "x"]] {
            let mut args = vec!["jarvis", "--reset"];
            args.extend_from_slice(other);
            assert!(parse_agent(&args).is_err(), "{args:?}");
        }
        assert!(parse_agent(&["--reset"])
            .unwrap_err()
            .contains("which agent"));
        let err = parse_agent(&["--text", "x"]).unwrap_err();
        assert!(err.contains("which agent"), "{err}");
    }

    #[test]
    fn agent_hook_is_hidden_and_takes_a_socket() {
        let cli = Cli::try_parse_from(["whisrs", "agent-hook", "--socket", "/tmp/w.sock"]).unwrap();
        assert!(matches!(
            cli.command,
            SubCmd::AgentHook { socket: Some(p) } if p == std::path::Path::new("/tmp/w.sock")
        ));
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("agent-hook"), "agent-hook must stay hidden");
        assert!(help.contains("agent"));
    }
}
