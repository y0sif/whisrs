//! The Copilot driver (GitHub Copilot CLI, `copilot --output-format json`):
//! one process per turn, on the process-per-turn base ([`super::process`]).
//! Verified on 1.0.103 against a local mock model (Copilot's offline BYOK
//! mode), not a real account.
//!
//! - **First turn:** `copilot --output-format json --session-id=<id>
//!   <permissions> [args…]`, the prompt on stdin, which Copilot's prompt
//!   mode reads when no `-p` is given (verbatim: a leading `--`, `$HOME` and
//!   quotes reached the model as typed, measured), so the user's words never
//!   show in `ps`. A prompt that starts with `/` is a Copilot command, not
//!   words for the model: prompt mode runs it through
//!   `autopilotSlashCommandParse` and `commands.invoke` (a slash command or
//!   a skill), and matches `/compact` after trimming the prompt (`cyo`), so
//!   no leading space or newline turns it back into words. whisrs passes the
//!   words as they are. **Later turns:** the same with `--resume=<id>`. whisrs
//!   mints the id, a UUID ([`new_session_id`]): `--session-id` with an id
//!   Copilot does not have starts a session with it, and with one it has
//!   continues it. The id is the agent's as soon as the first turn's process
//!   runs ([`Launch::session`]), pending until a text or a tool call shows
//!   the agent working in it, and every turn of a pending session passes
//!   `--session-id` again, so one stopped before Copilot made the session
//!   starts it. A confirmed one is resumed with `--resume`, which fails
//!   (stderr `No session, task, or name matched`, exit 1) when Copilot no
//!   longer has it, rather than starting an empty one under the same id.
//!   Only an id in UUID form is resumed: `--resume` also takes a prefix or
//!   a session name.
//! - **On every turn:** the permissions and `args` (Copilot keeps neither:
//!   an `--agent` of turn 1 was gone on turn 2, measured; a `--model` stays
//!   the session's either way, measured), and the instruction
//!   ([`INSTRUCTIONS_DIRS_VAR`]). Copilot refuses `--trust-folder` beside
//!   `--session-id` or `--resume`, and `--name` beside `--resume` or a
//!   `--session-id` it already has (measured), so those do not belong in
//!   `args`.
//! - **The voice instruction** is a custom instructions file of whisrs's
//!   own, `<data dir>/whisrs/copilot/<agent>/.github/instructions/
//!   whisrs-voice.instructions.md` ([`Copilot::write_instruction`]), whose
//!   directory is added to `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` on every turn.
//!   Copilot puts the file into the system prompt of every request of the
//!   turn, once, so it never shows as the user's words (measured on the
//!   first and on a resumed turn); a turn without the variable has none
//!   (measured), and nothing of it is kept in the session's history (only
//!   the system prompt snapshot in Copilot's own `events.jsonl`). An
//!   `AGENTS.md` or `copilot-instructions.md` in that directory is not
//!   loaded (measured). Written fresh each turn, so a changed instruction
//!   applies from the next turn after the restart. `--no-custom-instructions`
//!   in `args` turns it off with every other instruction file.
//! - **Permissions** ([`permission_args`]): `auto` is `--allow-all-tools`
//!   (commands and file changes in the dir run; a path outside the dir and
//!   the temp dir, and a URL, still need an approval, which nobody can give,
//!   so they are refused), `bypass` is `--yolo` (everything allowed), and
//!   `ask` is `--deny-tool=shell --deny-tool=write` (reads and searches run;
//!   commands and file changes are refused, also when the user's own config
//!   or `args` allow them, since a deny rule always wins, measured). Copilot's
//!   prompt mode never asks anyone: a request it cannot approve is refused
//!   (`code: "denied"`) and the model goes on without it, so the turn never
//!   waits for you.
//! - **Output:** one JSON object per line on stdout, `{type, data, agentId,
//!   id, timestamp, ...}`. A sub-agent's event is none of the turn's and is
//!   skipped, whatever its type ([`from_sub_agent`]: a top-level `agentId`,
//!   a `data.agentId` or the deprecated `data.parentToolCallId`, Copilot's
//!   own test). `assistant.message` is a message (`data.messageId`,
//!   `data.content`), the last one the reply; one with `toolRequests` is
//!   followed by a tool call at once, so its text is an interim right away.
//!   `tool.execution_start` is a tool call (`data.toolName`; an MCP tool as
//!   `mcp__<server>__<tool>`). A `session.error` fails the turn as
//!   Copilot's own outcome counts it (`pyo` and `oQt` in 1.0.103's
//!   `cli-main.js`): one with `errorType` `"hook"` never, one with
//!   `"model_call"` only when no message follows it (the agent went on and
//!   replied otherwise; Copilot's prompt mode exits 0 after one either way),
//!   any other always (the last word of a failed turn, after Copilot's own
//!   retries, exit 1). So does a `session.warning` Copilot exits 1 on
//!   ([`FAILING_WARNINGS`]), and an `abort`: an interrupted turn ends with
//!   `result` `exitCode: 0` and exits 0, so it would read as a success.
//!   `result` names the session. Nothing else matters to whisrs (deltas,
//!   model calls, retries, `session.info`).
//! - **Stop:** SIGINT or SIGTERM interrupts the turn (`abort`, then
//!   `result`), and the tool shell goes with it; the next turn resumes the
//!   session. A tool shell runs in a session of its own and outlives a
//!   SIGKILL of Copilot's process group (contract 3.6), which the base's
//!   scope and tree reach.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::process::{random_bytes, sentence, strip_ansi, Launch, StreamEvent, TurnHarness};
use super::{Agent, Permissions, SessionRef, TurnRequest};

/// What a parent Copilot session sets in the environment of the commands
/// it runs (its tool shells, measured on 1.0.103), and `COPILOT_ALLOW_ALL`,
/// which would approve every tool (and, as exactly `true`, trust the dir and
/// run its hooks) whatever `permissions` says. Removed from the `copilot`
/// child, besides what every driver removes. The `GIT_*` variables Copilot
/// also sets there pass: Copilot sets them again for its own tools
/// (measured), and a user's own `GIT_CONFIG_*` would go with them.
/// `COPILOT_HOME`, the tokens and the `COPILOT_PROVIDER_*` (BYOK) variables
/// pass.
pub const PARENT_SESSION_VARS: &[&str] = &[
    "COPILOT_ALLOW_ALL",
    "COPILOT_CLI",
    "COPILOT_AGENT_SESSION_ID",
    "COPILOT_CLI_BINARY_VERSION",
    "COPILOT_CLI_RESOLVED_DIST_DIR",
    "COPILOT_LOADER_PID",
    // Splits a prompt into several prompts at this text when set.
    "COPILOT_PROMPT_TURN_DELIMITER",
];

/// The variable Copilot reads extra custom instruction directories from (a
/// comma-separated list).
pub const INSTRUCTIONS_DIRS_VAR: &str = "COPILOT_CUSTOM_INSTRUCTIONS_DIRS";

/// The directory beside `agents.json` that holds each agent's instruction
/// directory.
pub const INSTRUCTIONS_ROOT: &str = "copilot";

/// The instruction file, inside an agent's instruction directory.
pub const INSTRUCTION_FILE: &str = ".github/instructions/whisrs-voice.instructions.md";

/// The `session.warning` types that fail a turn, as Copilot's exit code
/// counts them (`dyo` in 1.0.103's `cli-main.js`): a prompt a policy
/// blocked (no `user.message` follows), and a static context (system
/// prompt, instructions, tool definitions) too large for compaction to help.
pub const FAILING_WARNINGS: &[&str] = &["compaction_static_context_blocked", "policy_blocked"];

/// What a sign-in error's message may end with: Copilot's hint for its
/// interactive mode, replaced by the CLI command.
const SLASH_LOGIN_HINT: &str = "Run /login to sign in again.";

/// The Copilot harness.
pub struct Copilot {
    /// Where each agent's instruction directory goes: `<root>/<agent>`.
    instructions: PathBuf,
}

impl Copilot {
    /// A harness that keeps each agent's instruction under `root`.
    pub fn new(root: PathBuf) -> Self {
        Self { instructions: root }
    }

    /// The daemon's: the instructions beside its `agents.json` (`store`),
    /// in [`INSTRUCTIONS_ROOT`].
    pub fn beside_store(store: &Path) -> Self {
        Self::new(store.with_file_name(INSTRUCTIONS_ROOT))
    }

    /// `agent`'s instruction directory, the one Copilot is told about.
    pub fn instruction_dir(&self, agent: &Agent) -> PathBuf {
        self.instructions.join(&agent.name)
    }

    /// Write `agent`'s instruction file, unless it already says that: its
    /// directory, or `None` for a blank instruction (nothing to add).
    pub fn write_instruction(&self, agent: &Agent) -> std::io::Result<Option<PathBuf>> {
        let Some(contents) = instruction_file(&agent.instruction) else {
            return Ok(None);
        };
        let dir = self.instruction_dir(agent);
        let path = dir.join(INSTRUCTION_FILE);
        if std::fs::read_to_string(&path).is_ok_and(|current| current == contents) {
            return Ok(Some(dir));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let partial = path.with_extension("md.partial");
        std::fs::write(&partial, contents)?;
        std::fs::rename(&partial, &path)?;
        Ok(Some(dir))
    }
}

impl TurnHarness for Copilot {
    fn label(&self) -> &'static str {
        "Copilot"
    }

    fn launch(
        &self,
        agent: &Agent,
        session: &SessionRef,
        request: &TurnRequest,
    ) -> Result<Launch, String> {
        // A pending session may not exist in Copilot yet: started over.
        let (session_id, first) = match session.session_id.as_deref() {
            None => (new_session_id(), true),
            Some(id) if is_session_id(id) => (id.to_string(), session.pending),
            Some(id) => {
                return Err(format!(
                    "Agent '{}': its session {id} is not a Copilot session id, so whisrs does \
                     not resume it. `whisrs agent {} --new` starts a new session.",
                    agent.name, agent.name
                ))
            }
        };
        let mut env = Vec::new();
        let dir = self.write_instruction(agent).map_err(|e| {
            format!(
                "Agent '{}': cannot write its voice instruction for Copilot to {}: {e}",
                agent.name,
                self.instruction_dir(agent).join(INSTRUCTION_FILE).display()
            )
        })?;
        if let Some(dir) = dir {
            let existing = std::env::var(INSTRUCTIONS_DIRS_VAR).ok();
            let dirs = instructions_dirs(existing.as_deref(), &dir).map_err(|e| {
                format!(
                    "Agent '{}': Copilot cannot be given its voice instruction: {e}",
                    agent.name
                )
            })?;
            env.push((INSTRUCTIONS_DIRS_VAR.to_string(), dirs));
        }
        Ok(Launch {
            args: turn_args(agent, &session_id, first),
            stdin: Some(request.text.clone()),
            env,
            session: Some(session_id),
        })
    }

    fn strips_env(&self, name: &str) -> bool {
        PARENT_SESSION_VARS.contains(&name)
    }

    fn events(&self, line: &str) -> Vec<StreamEvent> {
        parse_line(line)
    }

    /// A session Copilot no longer has, a missing GitHub sign-in, and else
    /// Copilot's own `Error: ...` line. Its usage errors (`error: ...`) are
    /// left to the base.
    fn stderr_failure(&self, agent: &Agent, stderr: &[String]) -> Option<String> {
        let lines: Vec<String> = stderr
            .iter()
            .map(|l| strip_ansi(l).trim().to_string())
            .collect();
        if lines
            .iter()
            .any(|l| l.contains("No session, task, or name matched"))
        {
            return Some(format!(
                "Agent '{}': Copilot no longer has this agent's session, so it cannot continue \
                 it. `whisrs agent {} --new` starts a new one.",
                agent.name, agent.name
            ));
        }
        if lines
            .iter()
            .any(|l| l == "Error: No authentication information found.")
        {
            return Some(format!(
                "Agent '{}': Copilot is not signed in to GitHub. Run `copilot login` in a \
                 terminal, or give whisrsd a token in COPILOT_GITHUB_TOKEN, then try again.",
                agent.name
            ));
        }
        if let Some(at) = lines
            .iter()
            .position(|l| l.starts_with("Error: Authentication token found but could not"))
        {
            let why = lines[at + 1..]
                .iter()
                .find(|l| !l.is_empty())
                .map(|l| format!(" ({})", l.trim_end_matches('.')))
                .unwrap_or_default();
            return Some(format!(
                "Agent '{}': Copilot could not check its GitHub sign-in{why}. Check the network, \
                 or run `copilot login` again.",
                agent.name
            ));
        }
        let error = lines.iter().rev().find_map(|l| l.strip_prefix("Error: "))?;
        Some(format!(
            "Agent '{}': Copilot failed: {}",
            agent.name,
            sentence(error)
        ))
    }
}

/// The arguments of a turn in `session_id`; `first` for a turn that may
/// start the session (the first, or one of a pending session). The prompt
/// follows on stdin.
pub fn turn_args(agent: &Agent, session_id: &str, first: bool) -> Vec<String> {
    let mut args: Vec<String> = ["--output-format", "json"].map(String::from).to_vec();
    args.push(if first {
        format!("--session-id={session_id}")
    } else {
        format!("--resume={session_id}")
    });
    args.extend(
        permission_args(agent.permissions)
            .iter()
            .map(|a| a.to_string()),
    );
    args.extend(agent.args.iter().cloned());
    args
}

/// The flags of a harness-neutral setting, on every turn (Copilot keeps
/// none of them).
pub fn permission_args(permissions: Permissions) -> &'static [&'static str] {
    match permissions {
        Permissions::Auto => &["--allow-all-tools"],
        Permissions::Bypass => &["--yolo"],
        Permissions::Ask => &["--deny-tool=shell", "--deny-tool=write"],
    }
}

/// The instruction file for `instruction`: front matter that applies it
/// to every file (Copilot 1.0.103 loads the file without it as well, and
/// shows the front matter to the model as it is), then the instruction.
/// `None` for a blank instruction.
pub fn instruction_file(instruction: &str) -> Option<String> {
    let instruction = instruction.trim();
    (!instruction.is_empty()).then(|| format!("---\napplyTo: \"**\"\n---\n{instruction}\n"))
}

/// `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` with `dir` added after the daemon's
/// own (`existing`). An error says why `dir` cannot be in the list.
pub fn instructions_dirs(existing: Option<&str>, dir: &Path) -> Result<String, String> {
    let Some(dir) = dir.to_str() else {
        return Err(format!(
            "the path {} is not UTF-8, which {INSTRUCTIONS_DIRS_VAR} cannot hold",
            dir.display()
        ));
    };
    if dir.contains(',') {
        return Err(format!(
            "the path {dir} has a comma, which separates the entries of {INSTRUCTIONS_DIRS_VAR}"
        ));
    }
    Ok(match existing.map(str::trim).filter(|e| !e.is_empty()) {
        Some(existing) => format!("{existing},{dir}"),
        None => dir.to_string(),
    })
}

/// A new session id: a random (version 4) UUID, lowercase.
pub fn new_session_id() -> String {
    session_id_from(random_bytes::<16>())
}

/// The version 4 UUID of `bytes`.
pub fn session_id_from(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Whether `id` has the form of a Copilot session id: a UUID, as Codex's
/// thread ids ([`super::codex::is_thread_id`]).
pub fn is_session_id(id: &str) -> bool {
    super::codex::is_thread_id(id)
}

/// The events of one `copilot --output-format json` line (see the module
/// doc). Nothing for a line whisrs does not need, a sub-agent's event, a
/// message with no id and anything that is not JSON.
pub fn parse_line(line: &str) -> Vec<StreamEvent> {
    let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    if from_sub_agent(&entry) {
        return Vec::new();
    }
    let data = entry.get("data");
    let field = |k: &str| data.and_then(|d| d.get(k)).and_then(Value::as_str);
    match entry.get("type").and_then(Value::as_str) {
        Some("assistant.message") => data.map(message_events).unwrap_or_default(),
        Some("tool.execution_start") => data
            .and_then(|d| tool_name(d, "toolName"))
            .map(StreamEvent::Tool)
            .into_iter()
            .collect(),
        Some("session.error") => match field("errorType") {
            Some("hook") => Vec::new(),
            Some("model_call") => vec![StreamEvent::RecoverableError(failure(data))],
            _ => vec![StreamEvent::Error(failure(data))],
        },
        Some("session.warning")
            if field("warningType").is_some_and(|w| FAILING_WARNINGS.contains(&w)) =>
        {
            vec![StreamEvent::Error(failure(data))]
        }
        Some("abort") => vec![StreamEvent::Error(interrupted(
            data.and_then(|d| d.get("reason")).and_then(Value::as_str),
        ))],
        Some("result") => entry
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| is_session_id(id))
            .map(|id| StreamEvent::Session(id.to_string()))
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether `entry` is a sub-agent's event, by Copilot's own test (`nl` and
/// `ms` in 1.0.103's `cli-main.js`): a non-empty top-level `agentId`, else
/// `data.agentId`, else the deprecated `data.parentToolCallId`.
pub fn from_sub_agent(entry: &Value) -> bool {
    let data = entry.get("data");
    [
        entry.get("agentId"),
        data.and_then(|d| d.get("agentId")),
        data.and_then(|d| d.get("parentToolCallId")),
    ]
    .into_iter()
    .any(|id| id.and_then(Value::as_str).is_some_and(|id| !id.is_empty()))
}

/// The events of a main-agent `assistant.message`: its text (an empty one
/// too, so an empty last message is no reply rather than an earlier one),
/// then, when it asks for tools, its first tool call, which releases the
/// text as an interim at once.
fn message_events(data: &Value) -> Vec<StreamEvent> {
    let Some(message) = data.get("messageId").and_then(Value::as_str) else {
        return Vec::new();
    };
    let mut events = vec![StreamEvent::Text {
        message: message.to_string(),
        text: data
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }];
    if let Some(tool) = data
        .get("toolRequests")
        .and_then(Value::as_array)
        .and_then(|requests| requests.iter().find_map(|r| tool_name(r, "name")))
    {
        events.push(StreamEvent::Tool(tool));
    }
    events
}

/// A tool's name as [`super::progress::activity_for_tool`] labels it: its
/// `key` field, and `mcp__<server>__<tool>` for an MCP server's tool.
fn tool_name(tool: &Value, key: &str) -> Option<String> {
    let name = tool.get(key).and_then(Value::as_str)?;
    let field = |k: &str| {
        tool.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    Some(match field("mcpServerName") {
        Some(server) => format!("mcp__{server}__{}", field("mcpToolName").unwrap_or(name)),
        None => name.to_string(),
    })
}

/// What a `session.error` (or a failing `session.warning`) says: its
/// message, and for one Copilot answers with a sign-in (`remediation:
/// "sign_in"`) the command that signs in.
pub fn failure(data: Option<&Value>) -> String {
    let field = |k: &str| {
        data.and_then(|d| d.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let message = field("message").unwrap_or("");
    if field("remediation") == Some("sign_in") {
        let message = message.trim_end_matches(SLASH_LOGIN_HINT).trim();
        let message = if message.is_empty() {
            "it is not signed in to GitHub, or its sign-in expired"
        } else {
            message
        };
        let stop = if message.ends_with(['.', '!', '?']) {
            ""
        } else {
            "."
        };
        return format!("{message}{stop} Run `copilot login` in a terminal, then try again");
    }
    if message.is_empty() {
        return "the turn failed with no message".to_string();
    }
    message.to_string()
}

/// What an `abort` with `reason` says.
pub fn interrupted(reason: Option<&str>) -> String {
    match reason {
        Some("autopilot_credit_limit") => {
            "it stopped at the AI credit limit set for the session".to_string()
        }
        _ => "it was interrupted before it replied".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::process::{outcome, Exit, StreamState};
    use crate::agent::progress::Feedback;
    use crate::agent::{spoken, Harness, TurnOutcome, TurnUpdate, TurnUpdates};
    use std::os::unix::process::ExitStatusExt;

    /// Real `copilot --output-format json` output (1.0.103, the mock model,
    /// `--allow-all-tools`): one message with an acknowledgement and a shell
    /// call, the call, then the reply. In every fixture the lines whisrs
    /// never reads that only list Copilot's skills (`session.skills_loaded`,
    /// with local paths) or tick (`session.background_tasks_changed`) were
    /// dropped, and the run's dir is shortened to `/proj`.
    const ACK_THEN_TOOL: &str = include_str!("../../tests/fixtures/copilot/ack-then-tool.jsonl");
    /// The next turn of that session (`--resume=<id>`): a reply that counts
    /// two user turns.
    const RESUMED_REPLY: &str = include_str!("../../tests/fixtures/copilot/resumed-reply.jsonl");
    /// `ask` (`--deny-tool=shell --deny-tool=write`): the shell call is
    /// denied by the rule, the model goes on to its reply.
    const TOOL_DENIED: &str = include_str!("../../tests/fixtures/copilot/tool-denied.jsonl");
    /// A `view` of a file, then the reply.
    const VIEW_FILE: &str = include_str!("../../tests/fixtures/copilot/view-file.jsonl");
    /// The model provider answered HTTP 400: `session.error`, exit 1.
    const MODEL_ERROR: &str = include_str!("../../tests/fixtures/copilot/model-error.jsonl");
    /// The BYOK provider answered HTTP 401: retries, then `session.error`.
    const PROVIDER_AUTH: &str = include_str!("../../tests/fixtures/copilot/provider-auth.jsonl");
    /// Nothing listens at the BYOK provider's URL: five retries, then
    /// `session.error`.
    const PROVIDER_DOWN: &str = include_str!("../../tests/fixtures/copilot/provider-down.jsonl");
    /// SIGINT during a shell call: `abort`, then `result` with exit code 0.
    const INTERRUPTED: &str = include_str!("../../tests/fixtures/copilot/interrupted.jsonl");
    /// `--resume=` of a session Copilot does not have: stderr only, exit 1.
    const RESUME_UNKNOWN: &str = include_str!("../../tests/fixtures/copilot/resume-unknown.stderr");
    /// No GitHub sign-in: stderr only, exit 1. Not run (it would need
    /// Copilot without BYOK, which reaches GitHub): the text of 1.0.103's
    /// `cli-main.js`, as it writes it.
    const NOT_SIGNED_IN: &str = include_str!("../../tests/fixtures/copilot/not-signed-in.stderr");

    const SESSION: &str = "389b4e25-9c78-484d-a8fe-34304f61670b";

    fn agent(permissions: Permissions) -> Agent {
        Agent {
            name: "cp".into(),
            hotkey: None,
            new_session_hotkey: None,
            harness: Harness::Copilot,
            dir: PathBuf::from("/proj"),
            permissions,
            command: "copilot".into(),
            args: vec!["--model".into(), "gpt-5.4".into()],
            instruction: "Answer by voice.".into(),
            feedback: Feedback::Brief,
        }
    }

    fn request(text: &str) -> TurnRequest {
        TurnRequest {
            text: text.into(),
            started_at: chrono::Utc::now(),
        }
    }

    fn resumed(id: &str) -> SessionRef {
        SessionRef {
            session_id: Some(id.into()),
            bg_id: None,
            transcript_path: None,
            pending: false,
        }
    }

    fn pending(id: &str) -> SessionRef {
        SessionRef {
            pending: true,
            ..resumed(id)
        }
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn status(code: i32) -> Exit {
        Exit::Status(std::process::ExitStatus::from_raw(code << 8))
    }

    /// A harness writing into a fresh directory of its own, and its root.
    fn harness() -> (Copilot, PathBuf, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("copilot");
        (Copilot::new(root.clone()), root, tmp)
    }

    /// `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` for `dir`, after whatever the test
    /// process has.
    fn dirs_for(dir: &Path) -> String {
        instructions_dirs(std::env::var(INSTRUCTIONS_DIRS_VAR).ok().as_deref(), dir).unwrap()
    }

    #[test]
    fn a_first_turn_names_a_new_session_and_sends_the_words_on_stdin() {
        let (h, root, _tmp) = harness();
        for (permissions, flags) in [
            (Permissions::Auto, &["--allow-all-tools"][..]),
            (Permissions::Bypass, &["--yolo"][..]),
            (
                Permissions::Ask,
                &["--deny-tool=shell", "--deny-tool=write"][..],
            ),
        ] {
            let launch = h
                .launch(
                    &agent(permissions),
                    &SessionRef::default(),
                    &request("fix the build"),
                )
                .unwrap();
            let id = launch.session.clone().unwrap();
            assert!(is_session_id(&id));
            let mut want = strings(&["--output-format", "json"]);
            want.push(format!("--session-id={id}"));
            want.extend(strings(flags));
            want.extend(strings(&["--model", "gpt-5.4"]));
            assert_eq!(launch.args, want, "{permissions:?}");
            // The words alone, on stdin: never in argv, never with the
            // instruction.
            assert_eq!(launch.stdin.as_deref(), Some("fix the build"));
            assert!(!launch.args.iter().any(|a| a.contains("fix the build")));
            assert!(!launch.args.iter().any(|a| a == "-p" || a == "--prompt"));
            // The instruction goes in through its directory.
            let dir = root.join("cp");
            assert_eq!(
                launch.env,
                [(INSTRUCTIONS_DIRS_VAR.to_string(), dirs_for(&dir))]
            );
        }
    }

    #[test]
    fn a_later_turn_resumes_the_session_with_the_same_options() {
        let (h, _root, _tmp) = harness();
        for permissions in [Permissions::Auto, Permissions::Bypass, Permissions::Ask] {
            let a = agent(permissions);
            let first = h.launch(&a, &SessionRef::default(), &request("x")).unwrap();
            let next = h
                .launch(&a, &resumed(SESSION), &request("and the tests"))
                .unwrap();
            // Only the session flag differs: the permissions and args again.
            let mut want = first.args.clone();
            want[2] = format!("--resume={SESSION}");
            assert_eq!(next.args, want, "{permissions:?}");
            assert_eq!(next.stdin.as_deref(), Some("and the tests"));
            assert_eq!(next.session.as_deref(), Some(SESSION));
            assert_eq!(next.env, first.env);
        }
    }

    /// A pending session (its first turn only failed, or was stopped or cut
    /// off before the agent worked in it) passes `--session-id` again: it
    /// starts the session if Copilot never made it, and continues it if it
    /// did. `--resume` would fail on one it never made.
    #[test]
    fn a_pending_session_is_named_again_not_resumed() {
        let (h, _root, _tmp) = harness();
        let again = h
            .launch(
                &agent(Permissions::Auto),
                &pending(SESSION),
                &request("fix it"),
            )
            .unwrap();
        assert_eq!(again.args[2], format!("--session-id={SESSION}"));
        assert_eq!(again.session.as_deref(), Some(SESSION));
    }

    #[test]
    fn only_a_uuid_is_resumed() {
        let (h, _root, _tmp) = harness();
        for id in ["my-session", "389b4e2", "ses_e90ef9da1ffe00000000000000"] {
            let err = h
                .launch(&agent(Permissions::Auto), &resumed(id), &request("x"))
                .unwrap_err();
            assert!(
                err.contains("is not a Copilot session id")
                    && err.contains("whisrs agent cp --new")
            );
            assert!(!err.contains('\u{2014}'));
        }
    }

    #[test]
    fn session_ids_are_version_4_uuids() {
        assert_eq!(
            session_id_from([0; 16]),
            "00000000-0000-4000-8000-000000000000"
        );
        assert_eq!(
            session_id_from([0xff; 16]),
            "ffffffff-ffff-4fff-bfff-ffffffffffff"
        );
        let (a, b) = (new_session_id(), new_session_id());
        assert_ne!(a, b);
        for id in [&a, &b] {
            assert!(is_session_id(id));
            assert_eq!(&id[14..15], "4");
            assert!("89ab".contains(&id[19..20]));
        }
    }

    #[test]
    fn the_instruction_file_is_written_once_and_kept_up_to_date() {
        let (h, root, _tmp) = harness();
        let mut a = agent(Permissions::Auto);
        let dir = h.write_instruction(&a).unwrap().unwrap();
        assert_eq!(dir, root.join("cp"));
        let path = dir.join(INSTRUCTION_FILE);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "---\napplyTo: \"**\"\n---\nAnswer by voice.\n"
        );
        // The same instruction leaves the file alone.
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        h.write_instruction(&a).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        // A changed one replaces it; nothing partial is left.
        a.instruction = "Be terse.".into();
        h.write_instruction(&a).unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .ends_with("---\nBe terse.\n"));
        let files: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(files, ["whisrs-voice.instructions.md"]);
        // A blank instruction adds no directory.
        a.instruction = "  ".into();
        assert_eq!(h.write_instruction(&a).unwrap(), None);
        let launch = h.launch(&a, &SessionRef::default(), &request("x")).unwrap();
        assert!(launch.env.is_empty());
        assert_eq!(instruction_file(" "), None);
    }

    #[test]
    fn an_unwritable_instruction_fails_the_turn_with_its_path() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, "").unwrap();
        let h = Copilot::new(file.clone());
        let err = h
            .launch(
                &agent(Permissions::Auto),
                &SessionRef::default(),
                &request("x"),
            )
            .unwrap_err();
        assert!(
            err.starts_with("Agent 'cp': cannot write its voice instruction for Copilot to ")
                && err.contains("whisrs-voice.instructions.md")
        );
    }

    #[test]
    fn the_instruction_dir_joins_the_daemons_own_list() {
        let dir = Path::new("/data/whisrs/copilot/cp");
        assert_eq!(
            instructions_dirs(None, dir).unwrap(),
            "/data/whisrs/copilot/cp"
        );
        assert_eq!(
            instructions_dirs(Some(" "), dir).unwrap(),
            "/data/whisrs/copilot/cp"
        );
        assert_eq!(
            instructions_dirs(Some("/team/rules,/more"), dir).unwrap(),
            "/team/rules,/more,/data/whisrs/copilot/cp"
        );
        let err = instructions_dirs(None, Path::new("/odd,dir/cp")).unwrap_err();
        assert!(err.contains("has a comma"), "{err}");
        assert_eq!(
            Copilot::beside_store(Path::new("/data/whisrs/agents.json"))
                .instruction_dir(&agent(Permissions::Auto)),
            PathBuf::from("/data/whisrs/copilot/cp")
        );
    }

    #[test]
    fn only_a_parent_sessions_variables_and_allow_all_are_stripped() {
        let (h, _root, _tmp) = harness();
        for name in PARENT_SESSION_VARS {
            assert!(h.strips_env(name), "{name}");
        }
        for name in [
            "COPILOT_HOME",
            "COPILOT_GITHUB_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "COPILOT_PROVIDER_BASE_URL",
            "COPILOT_PROVIDER_API_KEY",
            "COPILOT_OFFLINE",
            "COPILOT_MODEL",
            INSTRUCTIONS_DIRS_VAR,
            "GIT_CONFIG_COUNT",
            "GIT_EDITOR",
            "PATH",
            "HOME",
        ] {
            assert!(!h.strips_env(name), "{name}");
        }
    }

    /// Run a fixture through the parser and the base's state, as a turn
    /// would: the updates sent and the state at the end.
    fn replay(fixture: &str, session: SessionRef) -> (Vec<TurnUpdate>, StreamState) {
        let (updates, mut rx) = TurnUpdates::channel();
        let mut state = StreamState::for_session(&session);
        for line in fixture.lines() {
            for event in parse_line(line) {
                state.event(event, &updates);
            }
        }
        let mut sent = Vec::new();
        while let Ok(update) = rx.try_recv() {
            sent.push(update);
        }
        (sent, state)
    }

    #[test]
    fn an_acknowledgement_with_a_tool_call_is_an_interim_at_once() {
        let lines: Vec<&str> = ACK_THEN_TOOL.lines().collect();
        // The message with the acknowledgement and the shell call: its text,
        // then the call, in one line.
        assert_eq!(
            parse_line(lines[9]),
            [
                StreamEvent::Text {
                    message: "3052f946-2fdb-49d3-8e9a-3560672df32b".into(),
                    text: "<spoken>On it.</spoken> Let me run that.".into(),
                },
                StreamEvent::Tool("bash".into()),
            ]
        );
        assert_eq!(parse_line(lines[10]), [StreamEvent::Tool("bash".into())]);
        assert_eq!(
            parse_line(lines[26]),
            [StreamEvent::Session(SESSION.into())]
        );
        // The first turn's id, reported pending as it started: the text
        // confirms it.
        let (sent, state) = replay(ACK_THEN_TOOL, pending(SESSION));
        assert_eq!(
            sent,
            [
                TurnUpdate::Session(resumed(SESSION)),
                TurnUpdate::Activity("Running a command".into()),
                TurnUpdate::Interim("<spoken>On it.</spoken> Let me run that.".into()),
            ]
        );
        let reply = state.reply().unwrap();
        assert_eq!(
            reply,
            "Finished the tool step. <spoken>The tool step is done.</spoken>"
        );
        assert_eq!(spoken::spoken_reply(&reply), "The tool step is done.");
        assert_eq!(
            outcome(
                &agent(Permissions::Auto),
                &Copilot::new(PathBuf::new()),
                &state,
                &[],
                &status(0)
            ),
            TurnOutcome::Reply(reply)
        );
    }

    #[test]
    fn a_resumed_turn_replies_in_its_own_session() {
        let (sent, state) = replay(RESUMED_REPLY, resumed(SESSION));
        // Its own session: nothing to report, no tool, no interim.
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(
            state.reply().as_deref(),
            Some("pong 2 <spoken>Pong number 2.</spoken>")
        );
    }

    #[test]
    fn a_denied_call_and_a_read_are_activities() {
        let (sent, state) = replay(TOOL_DENIED, SessionRef::default());
        // `result` names the session last.
        assert_eq!(
            sent,
            [
                TurnUpdate::Activity("Running a command".into()),
                TurnUpdate::Session(resumed("7ba37a5b-1047-4cc3-9f06-96fec1ad5cad")),
            ]
        );
        assert_eq!(
            state.reply().as_deref(),
            Some("Finished the tool step. <spoken>The tool step is done.</spoken>")
        );
        let (sent, _) = replay(VIEW_FILE, resumed("59d8f185-3f80-4349-bc2c-006dbe8c5794"));
        assert_eq!(sent, [TurnUpdate::Activity("Reading files".into())]);
    }

    /// Shapes the mock cannot make, as the 1.0.103 event schema
    /// (`schemas/session-events.schema.json`) defines them.
    #[test]
    fn mcp_tools_sub_agents_and_sign_in_errors() {
        assert_eq!(
            parse_line(
                r#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"whisrs-list_recordings","mcpServerName":"whisrs","mcpToolName":"list_recordings","arguments":{}}}"#
            ),
            [StreamEvent::Tool("mcp__whisrs__list_recordings".into())]
        );
        // A sub-agent's message is not the turn's (never its reply), nor is
        // its tool call: 1.0.103 names the sub-agent in a top-level
        // `agentId`.
        assert!(parse_line(
            r#"{"type":"assistant.message","agentId":"explore-1","data":{"messageId":"m9","content":"sub-agent notes","toolRequests":[]},"id":"0b8f3c1e-6d2a-4e57-9a0c-2f4d8e1b7c63","timestamp":"2026-10-25T18:40:02.114Z","parentId":"7e1d4a90-3c5b-4f28-b6e1-9d0a2c4f8b17"}"#
        )
        .is_empty());
        assert!(parse_line(
            r#"{"type":"tool.execution_start","agentId":"explore-1","data":{"toolCallId":"c2","toolName":"grep","arguments":{"pattern":"TODO"}},"id":"5a2c9e7f-1b4d-4c80-8f3a-6e0d2b9c1a45","timestamp":"2026-10-25T18:40:02.120Z","parentId":"0b8f3c1e-6d2a-4e57-9a0c-2f4d8e1b7c63"}"#
        )
        .is_empty());
        // The deprecated `data.parentToolCallId` still marks one.
        assert!(parse_line(
            r#"{"type":"assistant.message","data":{"messageId":"m9","content":"sub-agent notes","toolRequests":[],"parentToolCallId":"call_task"}}"#
        )
        .is_empty());
        // A tool request from an MCP server, in a message.
        assert_eq!(
            parse_line(
                r#"{"type":"assistant.message","data":{"messageId":"m1","content":"","toolRequests":[{"toolCallId":"c3","name":"github-list_issues","mcpServerName":"github","mcpToolName":"list_issues"}]}}"#
            ),
            [
                StreamEvent::Text {
                    message: "m1".into(),
                    text: String::new()
                },
                StreamEvent::Tool("mcp__github__list_issues".into()),
            ]
        );
        // An empty message asking for no tool: still an empty Text of its
        // message, so an empty last message is no reply.
        assert_eq!(
            parse_line(
                r#"{"type":"assistant.message","data":{"messageId":"m2","content":"","toolRequests":[]}}"#
            ),
            [StreamEvent::Text {
                message: "m2".into(),
                text: String::new()
            }]
        );
        // A GitHub sign-in Copilot refused: its message, and the command.
        let [StreamEvent::Error(message)] = &parse_line(
            r#"{"type":"session.error","data":{"errorType":"authentication","message":"Your GitHub token has expired. Run /login to sign in again.","statusCode":401,"remediation":"sign_in"}}"#,
        )[..] else {
            panic!("session.error fails the turn");
        };
        assert_eq!(
            message,
            "Your GitHub token has expired. Run `copilot login` in a terminal, then try again"
        );
        assert_eq!(
            failure(Some(
                &serde_json::json!({"message": "", "remediation": "sign_in"})
            )),
            "it is not signed in to GitHub, or its sign-in expired. Run `copilot login` in a \
             terminal, then try again"
        );
        assert_eq!(failure(None), "the turn failed with no message");
    }

    #[test]
    fn a_model_or_provider_error_fails_the_turn_with_its_message() {
        let a = agent(Permissions::Auto);
        let h = Copilot::new(PathBuf::new());
        for (fixture, want) in [
            (
                MODEL_ERROR,
                "Agent 'cp': Copilot failed: 400 mock: this request is refused.",
            ),
            (
                PROVIDER_AUTH,
                "Agent 'cp': Copilot failed: Authentication failed with provider at \
                 http://127.0.0.1:18961/v1 (HTTP 401). Check your COPILOT_PROVIDER_API_KEY, \
                 COPILOT_PROVIDER_API_KEY_COMMAND, or COPILOT_PROVIDER_BEARER_TOKEN.",
            ),
            (
                PROVIDER_DOWN,
                "Agent 'cp': Copilot failed: Could not connect to local model provider at \
                 http://127.0.0.1:18961/v1. Check that the service is running and the port is \
                 correct.",
            ),
        ] {
            let (sent, state) = replay(fixture, SessionRef::default());
            // Only the session (from `result`): no text, no tool. The retries
            // before the error say nothing.
            assert!(
                sent.iter().all(|u| matches!(u, TurnUpdate::Session(_))),
                "{sent:?}"
            );
            let TurnOutcome::Failed(message) = outcome(&a, &h, &state, &[], &status(1)) else {
                panic!("session.error fails the turn");
            };
            assert_eq!(message, want);
            assert!(!message.contains('\u{2014}'));
        }
        // A model error leaves a pending first turn pending: the next turn
        // names the session again rather than resuming it.
        let (sent, state) = replay(MODEL_ERROR, pending("8192c51a-dc0f-4d06-93ff-9e8c1a455960"));
        assert!(sent.is_empty(), "{sent:?}");
        let TurnOutcome::Failed(message) = outcome(&a, &h, &state, &[], &status(1)) else {
            panic!("session.error fails the turn");
        };
        assert_eq!(
            spoken::spoken_error(&message),
            "Copilot failed: 400 mock: this request is refused."
        );
    }

    /// `ACK_THEN_TOOL` with `extra` put in before its line that holds
    /// `before`.
    fn ack_then_tool_with(before: &str, extra: &[&str]) -> String {
        let mut lines: Vec<&str> = ACK_THEN_TOOL.lines().collect();
        let at = lines.iter().position(|l| l.contains(before)).unwrap();
        let tail = lines.split_off(at);
        lines.extend_from_slice(extra);
        lines.extend(tail);
        lines.join("\n")
    }

    /// The fixture's final message, which every turn below replies with
    /// unless it fails.
    const FINAL_MESSAGE: &str = "\"content\":\"Finished the tool step.";
    /// Its `result` line, the turn's last.
    const RESULT: &str = "\"type\":\"result\"";

    /// A `model_call` error after the shell call (as 1.0.103's schema has
    /// a `session.error`, the user message's id as its origin).
    const MODEL_CALL_ERROR: &str = r#"{"type":"session.error","data":{"errorType":"model_call","message":"Model call failed: the stream ended early","originatingMessageId":"ea7374f6-3fdc-4072-8e6c-c5a651dced84"},"id":"5e0c3d1a-7b7e-4f0e-9a51-0c6f1b2a9d10","timestamp":"2026-10-25T18:38:40.650Z","parentId":"1ddbd953-571e-43e2-8c9f-9d5250a0c390"}"#;

    fn turn_outcome(fixture: &str, exit: i32) -> TurnOutcome {
        let (_, state) = replay(fixture, SessionRef::default());
        outcome(
            &agent(Permissions::Auto),
            &Copilot::new(PathBuf::new()),
            &state,
            &[],
            &status(exit),
        )
    }

    fn the_reply() -> TurnOutcome {
        TurnOutcome::Reply("Finished the tool step. <spoken>The tool step is done.</spoken>".into())
    }

    /// A `model_call` error is recoverable: Copilot's own turn outcome
    /// (`oQt` in 1.0.103's `cli-main.js`) lets a later message of the main
    /// agent clear it, and its prompt mode exits 0 after one (`pyo`).
    #[test]
    fn a_model_call_error_is_recoverable() {
        assert_eq!(
            parse_line(MODEL_CALL_ERROR),
            [StreamEvent::RecoverableError(
                "Model call failed: the stream ended early".into()
            )]
        );
    }

    /// What Copilot's outcome ignores (`oQt`, `pyo`): an error of a hook,
    /// and any error or abort of a sub-agent. The main agent's reply stands.
    #[test]
    fn hook_and_sub_agent_errors_never_fail_the_turn() {
        let hook = r#"{"type":"session.error","data":{"errorType":"hook","message":"Hook preToolUse failed with exit code 1","originatingMessageId":"ea7374f6-3fdc-4072-8e6c-c5a651dced84"},"id":"9c41e2b7-0f3a-4d6e-8b15-7a2d9e0c4f81","timestamp":"2026-10-25T18:38:40.640Z","parentId":"1ddbd953-571e-43e2-8c9f-9d5250a0c390"}"#;
        assert!(parse_line(hook).is_empty());
        let sub_agent = [
            r#"{"type":"assistant.message","agentId":"explore-1","data":{"messageId":"9f3e6a21-4c7d-4b0e-a2f8-5d1c3b7e9a04","content":"I found nothing.","toolRequests":[]},"id":"2d7b0e4c-8a1f-4e93-b6c2-0f5a9d3e7b18","timestamp":"2026-10-25T18:38:40.652Z","parentId":"1ddbd953-571e-43e2-8c9f-9d5250a0c390"}"#,
            r#"{"type":"session.error","agentId":"explore-1","data":{"errorType":"query","message":"400 the sub-agent's request is refused"},"id":"6b1d8f3a-2e4c-4a07-9d5e-8c0f1a6b3e72","timestamp":"2026-10-25T18:38:40.655Z","parentId":"2d7b0e4c-8a1f-4e93-b6c2-0f5a9d3e7b18"}"#,
            r#"{"type":"session.error","agentId":"explore-1","data":{"errorType":"model_call","message":"Model call failed"},"id":"0e5c7a92-3d1b-4f68-a4e0-9b2d6c8f1a35","timestamp":"2026-10-25T18:38:40.656Z","parentId":"6b1d8f3a-2e4c-4a07-9d5e-8c0f1a6b3e72"}"#,
            r#"{"type":"abort","agentId":"explore-1","data":{"reason":"user_initiated"},"id":"4f8a2c6e-9b3d-4e17-8a05-c1d7e3b9f260","timestamp":"2026-10-25T18:38:40.658Z","parentId":"0e5c7a92-3d1b-4f68-a4e0-9b2d6c8f1a35"}"#,
            // The other ways Copilot's test names a sub-agent.
            r#"{"type":"session.error","data":{"agentId":"explore-1","errorType":"query","message":"refused"}}"#,
            r#"{"type":"abort","data":{"reason":"user_initiated","parentToolCallId":"call_task"}}"#,
        ];
        for line in sub_agent {
            assert!(parse_line(line).is_empty(), "{line}");
        }
        let mut extra = vec![hook];
        extra.extend(sub_agent);
        assert_eq!(
            turn_outcome(&ack_then_tool_with(FINAL_MESSAGE, &extra), 0),
            the_reply()
        );
        // A sub-agent's message after the reply is not the reply.
        assert_eq!(
            turn_outcome(&ack_then_tool_with(RESULT, &sub_agent[..1]), 0),
            the_reply()
        );
        // An empty `agentId` names no sub-agent: the main agent's error.
        assert_eq!(
            parse_line(
                r#"{"type":"session.error","agentId":"","data":{"agentId":"","parentToolCallId":"","errorType":"query","message":"refused"}}"#
            ),
            [StreamEvent::Error("refused".into())]
        );
        // Any other error of the main agent fails the turn, after a reply too.
        let query = r#"{"type":"session.error","data":{"errorType":"query","message":"400 refused","originatingMessageId":"ea7374f6-3fdc-4072-8e6c-c5a651dced84"},"id":"3a9e1c5b-7d2f-4b84-9e06-d8c2a4f7b103","timestamp":"2026-10-25T18:38:40.681Z","parentId":"213d7a30-4eef-4d87-869a-8058fb68413d"}"#;
        assert_eq!(
            turn_outcome(&ack_then_tool_with(RESULT, &[query]), 1),
            TurnOutcome::Failed("Agent 'cp': Copilot failed: 400 refused.".into())
        );
    }

    /// The warnings Copilot exits 1 on (`dyo`, `pyo`) fail the turn with
    /// their message; any other warning says nothing.
    #[test]
    fn a_policy_block_fails_the_turn() {
        let blocked = r#"{"type":"session.warning","data":{"warningType":"policy_blocked","message":"This prompt was blocked by your organization's Copilot policy.","originatingMessageId":"ea7374f6-3fdc-4072-8e6c-c5a651dced84"},"id":"8d2f4b6a-1c3e-4a59-b7d0-e9f1a3c5b827","timestamp":"2026-10-25T18:38:40.571Z","parentId":"0db94a7a-18fa-4b51-84b4-6e53074e4417"}"#;
        assert_eq!(
            parse_line(blocked),
            [StreamEvent::Error(
                "This prompt was blocked by your organization's Copilot policy.".into()
            )]
        );
        let static_context = r#"{"type":"session.warning","data":{"warningType":"compaction_static_context_blocked","message":"Static context is too large."}}"#;
        assert_eq!(
            parse_line(static_context),
            [StreamEvent::Error("Static context is too large.".into())]
        );
        assert_eq!(
            turn_outcome(&ack_then_tool_with(RESULT, &[static_context]), 1),
            TurnOutcome::Failed("Agent 'cp': Copilot failed: Static context is too large.".into())
        );
        assert!(parse_line(
            r#"{"type":"session.warning","agentId":"explore-1","data":{"warningType":"policy_blocked","message":"blocked"}}"#
        )
        .is_empty());
    }

    /// An interrupted turn ends with `result` exit code 0 and exits 0: the
    /// `abort` before it fails it, so the acknowledgement held before the
    /// call is never taken for the reply.
    #[test]
    fn an_interrupted_turn_is_no_success() {
        assert_eq!(
            turn_outcome(INTERRUPTED, 0),
            TurnOutcome::Failed(
                "Agent 'cp': Copilot failed: it was interrupted before it replied.".into()
            )
        );
        assert_eq!(
            interrupted(Some("autopilot_credit_limit")),
            "it stopped at the AI credit limit set for the session"
        );
    }

    #[test]
    fn stderr_failures_say_what_to_do() {
        let a = agent(Permissions::Auto);
        let h = Copilot::new(PathBuf::new());
        let lines = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
        let empty = StreamState::new(Some(SESSION.into()));
        let failed = |stderr: &[String]| match outcome(&a, &h, &empty, stderr, &status(1)) {
            TurnOutcome::Failed(message) => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            failed(&lines(RESUME_UNKNOWN)),
            "Agent 'cp': Copilot no longer has this agent's session, so it cannot continue it. \
             `whisrs agent cp --new` starts a new one."
        );
        let signed_out = failed(&lines(NOT_SIGNED_IN));
        assert_eq!(
            signed_out,
            "Agent 'cp': Copilot is not signed in to GitHub. Run `copilot login` in a terminal, \
             or give whisrsd a token in COPILOT_GITHUB_TOKEN, then try again."
        );
        assert_eq!(
            spoken::spoken_error(&signed_out),
            "Copilot is not signed in to GitHub."
        );
        // A token Copilot could not check (the text of 1.0.103's bundle).
        let unchecked = vec![
            "Error: Authentication token found but could not be validated.".to_string(),
            String::new(),
            "  Failed to fetch PAT user login (401): GitHub returned: Bad credentials".to_string(),
            String::new(),
            "Your token may still be valid. Check your network connection and try again."
                .to_string(),
        ];
        assert_eq!(
            failed(&unchecked),
            "Agent 'cp': Copilot could not check its GitHub sign-in (Failed to fetch PAT user \
             login (401): GitHub returned: Bad credentials). Check the network, or run `copilot \
             login` again."
        );
        // Another `Error:` line: Copilot's own words.
        let pat = vec![
            "Error: Classic Personal Access Tokens (ghp_) are not supported by Copilot.".into(),
            "The GH_TOKEN environment variable contains a classic PAT.".to_string(),
        ];
        assert_eq!(
            failed(&pat),
            "Agent 'cp': Copilot failed: Classic Personal Access Tokens (ghp_) are not supported \
             by Copilot."
        );
        // A usage error is the base's.
        let usage = vec![
            "error: the argument '--resume [<value>]' cannot be used with '--name <name>'"
                .to_string(),
        ];
        assert_eq!(h.stderr_failure(&a, &usage), None);
        assert_eq!(
            failed(&usage),
            "Agent 'cp': Copilot failed: error: the argument '--resume [<value>]' cannot be used \
             with '--name <name>'."
        );
        for message in [failed(&lines(RESUME_UNKNOWN)), signed_out] {
            assert!(!message.contains('\u{2014}'), "{message}");
        }
    }

    #[test]
    fn anything_else_is_nothing() {
        for line in [
            "",
            "not json",
            "[1, 2]",
            r#"{"type":"user.message","data":{"content":"hi","messageId":"u1"}}"#,
            r#"{"type":"assistant.message_delta","data":{"messageId":"m1","deltaContent":"po"}}"#,
            r#"{"type":"assistant.turn_retry","data":{"turnId":"0","reason":"streaming_error"}}"#,
            r#"{"type":"session.info","data":{"infoType":"retry","message":"Retrying"}}"#,
            r#"{"type":"session.warning","data":{"warningType":"quota","message":"80% used"}}"#,
            r#"{"type":"tool.execution_complete","data":{"toolCallId":"c1","success":true}}"#,
            r#"{"type":"assistant.message","data":{"content":"no id"}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"c1"}}"#,
            r#"{"type":"result","sessionId":"my-session","exitCode":0}"#,
        ] {
            assert!(parse_line(line).is_empty(), "{line}");
        }
        assert_eq!(
            parse_line(r#"{"type":"session.error","data":{}}"#),
            [StreamEvent::Error("the turn failed with no message".into())]
        );
    }
}
