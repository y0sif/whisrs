//! The Codex driver (Codex CLI, `codex exec --json`): one process per turn,
//! on the process-per-turn base ([`super::process`]). Verified on 0.164.0
//! against a local mock model, not a real account.
//!
//! - **First turn:** `codex exec --json <permissions> -c
//!   analytics.enabled=false -c developer_instructions=<instruction>
//!   [args…] --skip-git-repo-check -`, the prompt on stdin. **Later turns:**
//!   the same, then `resume <thread id> -`. Every option goes before
//!   `resume` (`--sandbox` and `--approve-for-me` are refused after it).
//!   `--skip-git-repo-check` because the dir may be any folder, and after
//!   `args`: a flag there ends an option of `args` that takes several
//!   values (`-i`/`--image`), which would take `resume`, the id and `-` as
//!   more values (checked on 0.164.0's argument parser; the flag may be
//!   given once only). Codex mints the thread id: the first line names it
//!   (`thread.started`), and a resumed turn's first line names the same id.
//!   Only an id in UUID form is taken or resumed: Codex would look anything
//!   else up as a thread name, and start a new thread when none has it. A
//!   turn that failed after `thread.started` keeps its thread (it resumes,
//!   measured): nothing is passed on the first turn only, so a failed one
//!   pins nothing, and the id is never pending.
//! - **On every turn:** `args` and the permissions, because `codex exec
//!   resume` keeps neither from the first turn (a `-m` model of turn 1 was
//!   gone on turn 2, measured; permission flags on a resumed turn apply to
//!   it, measured both ways), and the instruction ([`instruction_config`]).
//! - **Analytics off** ([`NO_ANALYTICS`]): with them on, `codex exec` sends
//!   usage metrics after the turn and exits only once they are sent, 4 to
//!   10 s after `turn.completed` (measured), and the reply waits for the
//!   exit; off, it exits at once. A `-c analytics.enabled=true` in `args`
//!   comes later and wins (measured).
//! - **The voice instruction** is a `developer` message
//!   (`developer_instructions`), so it never shows as the user's words. It
//!   is stored with the session's first turn; passed again on a resumed
//!   turn it is not repeated (one copy per request, measured), and a
//!   compaction, which drops it from a session that got it once, keeps it
//!   (measured). A changed instruction does not replace the stored one: it
//!   applies from the next `--new`, as with Claude and OpenCode (Copilot
//!   takes it on every turn). It replaces
//!   any `developer_instructions` of the user's Codex config or profile.
//! - **Permissions** ([`permission_args`]): `auto` is `--approve-for-me`
//!   (the workspace-write sandbox, writes in the dir; a command that needs
//!   more goes to Codex's automatic reviewer, an extra model call),
//!   `bypass` is `--dangerously-bypass-approvals-and-sandbox` (no sandbox,
//!   no reviews), and `ask` is `--sandbox read-only` (Codex reads and runs
//!   commands that only read; anything that writes is refused). `codex
//!   exec` never asks anyone: a request it cannot approve is refused and the
//!   model goes on without it, so the turn never waits for you.
//! - **Output:** one JSON object per line on stdout, `{type, ...}`.
//!   `thread.started` names the thread; `item.completed` of an
//!   `agent_message` is a message (its `item.id`, its `text`), the last one
//!   the reply, an empty one included (the turn then ended without a reply,
//!   never with an earlier message as one); `item.started` of a command,
//!   file change, MCP tool call, web
//!   search, plan update or helper agent is a tool call, printed when it
//!   starts; `turn.failed` fails the turn. Top-level `error` lines and
//!   `error` items are retries and warnings (a fatal one comes back in
//!   `turn.failed`). Nothing is taken from `item.completed` of a tool: an
//!   unfinished one is completed after the final message.
//! - **Stop:** SIGINT interrupts the turn and exits; the command it runs
//!   goes with it, and the next turn resumes the thread (measured).

use serde_json::Value;

use super::process::{sentence, strip_ansi, Launch, StreamEvent, TurnHarness};
use super::{Agent, Permissions, SessionRef, TurnRequest};

/// What a parent Codex session sets in the environment of the commands it
/// runs. Removed from the `codex` child, besides what every driver removes,
/// so a daemon started from inside a Codex session does not hand the agent
/// that session's thread or sandbox. `CODEX_HOME` and `CODEX_API_KEY`
/// pass.
pub const PARENT_SESSION_VARS: &[&str] = &[
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "CODEX_TOOL_CALL_ID",
    "CODEX_CI",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CODEX_VERSION",
    "CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS",
];

/// The daemon's own variables Codex would also read: whisrsd's log filter,
/// which would set Codex's (it logs to stderr at that level, measured).
pub const DAEMON_VARS: &[&str] = &["RUST_LOG"];

/// The `-c` override that keeps `codex exec` from sending usage metrics
/// after the turn, which delays its exit, and so the reply, by seconds.
pub const NO_ANALYTICS: &str = "analytics.enabled=false";

/// The API key Codex takes from the environment before any sign-in it has
/// saved (`codex exec` reads no other key variable). It passes to the
/// child.
pub const API_KEY_VAR: &str = "CODEX_API_KEY";

/// Whether the daemon's environment, and so the child's, has a key in
/// [`API_KEY_VAR`].
fn api_key_set() -> bool {
    std::env::var(API_KEY_VAR).is_ok_and(|key| !key.trim().is_empty())
}

/// The Codex harness.
pub struct Codex;

impl TurnHarness for Codex {
    fn label(&self) -> &'static str {
        "Codex"
    }

    fn launch(
        &self,
        agent: &Agent,
        session: &SessionRef,
        request: &TurnRequest,
    ) -> Result<Launch, String> {
        let thread = match session.session_id.as_deref() {
            None => None,
            Some(id) if is_thread_id(id) => Some(id),
            Some(id) => {
                return Err(format!(
                    "Agent '{}': its session {id} is not a Codex thread id, so whisrs does not \
                     resume it. `whisrs agent {} --new` starts a new session.",
                    agent.name, agent.name
                ))
            }
        };
        Ok(Launch {
            args: turn_args(agent, thread),
            stdin: Some(request.text.clone()),
            env: Vec::new(),
            // Codex names it: the first line.
            session: None,
        })
    }

    fn strips_env(&self, name: &str) -> bool {
        PARENT_SESSION_VARS.contains(&name) || DAEMON_VARS.contains(&name)
    }

    fn events(&self, line: &str) -> Vec<StreamEvent> {
        parse_line(line, api_key_set())
    }

    /// A thread Codex no longer has (its rollout deleted or archived), and
    /// else Codex's own error line (`Error: ...`), which comes after log
    /// lines that also say `ERROR`.
    fn stderr_failure(&self, agent: &Agent, stderr: &[String]) -> Option<String> {
        let lines: Vec<String> = stderr
            .iter()
            .map(|l| strip_ansi(l).trim().to_string())
            .collect();
        if lines
            .iter()
            .any(|l| l.contains("no rollout found for thread id"))
        {
            return Some(format!(
                "Agent '{}': Codex no longer has this agent's session, so it cannot continue it. \
                 `whisrs agent {} --new` starts a new one.",
                agent.name, agent.name
            ));
        }
        let error = lines.iter().rev().find_map(|l| l.strip_prefix("Error: "))?;
        Some(format!(
            "Agent '{}': Codex failed: {}",
            agent.name,
            sentence(error)
        ))
    }
}

/// The arguments of a turn: a new thread's (`thread` `None`), or one that
/// resumes `thread`. The prompt follows on stdin (`-`). `args` come after
/// whisrs's `-c` overrides, so one of theirs wins, and before
/// `--skip-git-repo-check`, which ends an option of theirs that takes
/// several values (see the module doc).
pub fn turn_args(agent: &Agent, thread: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["exec", "--json"].map(String::from).to_vec();
    args.extend(
        permission_args(agent.permissions)
            .iter()
            .map(|a| a.to_string()),
    );
    args.push("-c".to_string());
    args.push(NO_ANALYTICS.to_string());
    if let Some(config) = instruction_config(&agent.instruction) {
        args.push("-c".to_string());
        args.push(config);
    }
    args.extend(agent.args.iter().cloned());
    args.push("--skip-git-repo-check".to_string());
    if let Some(thread) = thread {
        args.push("resume".to_string());
        args.push(thread.to_string());
    }
    args.push("-".to_string());
    args
}

/// The flags of a harness-neutral setting, on every turn.
pub fn permission_args(permissions: Permissions) -> &'static [&'static str] {
    match permissions {
        Permissions::Auto => &["--approve-for-me"],
        Permissions::Bypass => &["--dangerously-bypass-approvals-and-sandbox"],
        Permissions::Ask => &["--sandbox", "read-only"],
    }
}

/// The `-c` value that makes `instruction` the session's developer
/// instructions, as a quoted TOML string (`-c` parses its value as TOML,
/// and an instruction that happens to parse as another TOML value would
/// not be a string). `None` for a blank instruction.
pub fn instruction_config(instruction: &str) -> Option<String> {
    let instruction = instruction.trim();
    (!instruction.is_empty())
        .then(|| format!("developer_instructions={}", toml_string(instruction)))
}

/// `text` as a TOML basic string: quotes, backslashes and control
/// characters escaped.
pub fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Whether `id` has the form of a Codex thread id: a UUID, 8-4-4-4-12 hex
/// digits.
pub fn is_thread_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// The events of one `codex exec --json` line (see the module doc);
/// `api_key` when the child has a key in [`API_KEY_VAR`] ([`failure`]).
/// Nothing for `turn.started`, `turn.completed`, `item.updated`, a
/// completed tool, reasoning, an `error` line or item, a message with no
/// id and anything that is not JSON.
pub fn parse_line(line: &str, api_key: bool) -> Vec<StreamEvent> {
    let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    let item = |key: &str| {
        entry
            .get("item")
            .and_then(|i| i.get(key))
            .and_then(Value::as_str)
    };
    let event = match entry.get("type").and_then(Value::as_str) {
        Some("thread.started") => entry
            .get("thread_id")
            .and_then(Value::as_str)
            .filter(|id| is_thread_id(id))
            .map(|id| StreamEvent::Session(id.to_string())),
        // An empty message is still the last one: the turn then ended
        // without a reply, rather than with an earlier message (spoken
        // already, as the interim before a tool call).
        Some("item.completed") if item("type") == Some("agent_message") => {
            item("id").map(|message| StreamEvent::Text {
                message: message.to_string(),
                text: item("text").unwrap_or("").to_string(),
            })
        }
        Some("item.started") => entry.get("item").and_then(tool_name).map(StreamEvent::Tool),
        Some("turn.failed") => Some(StreamEvent::Error(failure(
            entry
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or(""),
            api_key,
        ))),
        _ => None,
    };
    event.into_iter().collect()
}

/// The tool name of a started item, as [`super::progress::activity_for_tool`]
/// labels it: the item type, and `mcp__<server>__<tool>` for an MCP tool
/// call. `None` for an item that is no tool call.
fn tool_name(item: &Value) -> Option<String> {
    match item.get("type")?.as_str()? {
        kind @ ("command_execution" | "file_change" | "web_search" | "todo_list"
        | "collab_tool_call") => Some(kind.to_string()),
        "mcp_tool_call" => {
            let server = item.get("server")?.as_str()?;
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or("");
            Some(format!("mcp__{server}__{tool}"))
        }
        _ => None,
    }
}

/// What a `turn.failed` message says: the message of a provider's JSON
/// error body, and a refused sign-in as one, actionable. With `api_key`
/// (the child has [`API_KEY_VAR`], which Codex uses before its saved
/// sign-in) a 401 is that key, refused.
pub fn failure(message: &str, api_key: bool) -> String {
    let message = message.trim();
    let message = serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|body| {
            body.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| message.to_string());
    if message.contains("401 Unauthorized") {
        if api_key {
            return format!(
                "the API key in {API_KEY_VAR} was refused (401 Unauthorized). Fix it in \
                 whisrsd's environment, or remove it there to use the sign-in of `codex login`"
            );
        }
        return "it is not signed in, or its sign-in expired (401 Unauthorized). Run `codex \
                login` in a terminal, then try again"
            .to_string();
    }
    if message.is_empty() {
        return "the turn failed with no message".to_string();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::process::{outcome, Exit, StreamState};
    use crate::agent::progress::Feedback;
    use crate::agent::{spoken, Harness, TurnOutcome, TurnUpdate, TurnUpdates};
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;

    /// Real `codex exec --json` output (0.164.0, the mock model): an
    /// acknowledgement, a shell command, then the reply. The `error` item is
    /// the mock's unknown model name.
    const ACK_THEN_TOOL: &str = include_str!("../../tests/fixtures/codex/ack-then-tool.jsonl");
    /// The next turn of that thread (`resume <id>`): the same id, a reply.
    const RESUMED_REPLY: &str = include_str!("../../tests/fixtures/codex/resumed-reply.jsonl");
    /// The model provider answered HTTP 400: an `error` line, then
    /// `turn.failed`, exit 1.
    const TURN_FAILED: &str = include_str!("../../tests/fixtures/codex/turn-failed.jsonl");
    /// No sign-in (an empty `CODEX_HOME`): retries as `error` lines, then
    /// `turn.failed`, exit 1.
    const NOT_LOGGED_IN: &str = include_str!("../../tests/fixtures/codex/not-logged-in.jsonl");
    /// A patch Codex applied (a file change).
    const FILE_CHANGE: &str = include_str!("../../tests/fixtures/codex/file-change.jsonl");
    /// A web search, then the reply.
    const WEB_SEARCH: &str = include_str!("../../tests/fixtures/codex/web-search.jsonl");
    /// `resume` of a thread Codex does not have: stderr only, exit 1.
    const RESUME_UNKNOWN: &str = include_str!("../../tests/fixtures/codex/resume-unknown.stderr");

    const THREAD: &str = "01a17400-230f-79e2-877d-e271e75b56d9";

    fn agent(permissions: Permissions) -> Agent {
        Agent {
            name: "cx".into(),
            hotkey: None,
            new_session_hotkey: None,
            harness: Harness::Codex,
            dir: PathBuf::from("/proj"),
            permissions,
            command: "codex".into(),
            args: vec!["-m".into(), "gpt-5.1-codex".into()],
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

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn status(code: i32) -> Exit {
        Exit::Status(std::process::ExitStatus::from_raw(code << 8))
    }

    #[test]
    fn a_first_turn_starts_a_thread_with_everything_before_the_prompt() {
        for (permissions, flags) in [
            (Permissions::Auto, &["--approve-for-me"][..]),
            (
                Permissions::Bypass,
                &["--dangerously-bypass-approvals-and-sandbox"][..],
            ),
            (Permissions::Ask, &["--sandbox", "read-only"][..]),
        ] {
            let launch = Codex
                .launch(
                    &agent(permissions),
                    &SessionRef::default(),
                    &request("fix the build"),
                )
                .unwrap();
            let mut want = strings(&["exec", "--json"]);
            want.extend(strings(flags));
            want.extend(strings(&[
                "-c",
                "analytics.enabled=false",
                "-c",
                "developer_instructions=\"Answer by voice.\"",
                "-m",
                "gpt-5.1-codex",
                "--skip-git-repo-check",
                "-",
            ]));
            assert_eq!(launch.args, want, "{permissions:?}");
            // The words go in on stdin, alone: the instruction is not in them.
            assert_eq!(launch.stdin.as_deref(), Some("fix the build"));
            assert!(launch.env.is_empty());
            // Codex names the thread itself.
            assert_eq!(launch.session, None);
        }
    }

    #[test]
    fn a_later_turn_resumes_the_thread_with_the_same_options() {
        for permissions in [Permissions::Auto, Permissions::Bypass, Permissions::Ask] {
            let a = agent(permissions);
            let first = Codex
                .launch(&a, &SessionRef::default(), &request("x"))
                .unwrap();
            let next = Codex
                .launch(&a, &resumed(THREAD), &request("and the tests"))
                .unwrap();
            // Every option of the first turn, then `resume <id> -`.
            let mut want = first.args[..first.args.len() - 1].to_vec();
            want.extend(strings(&["resume", THREAD, "-"]));
            assert_eq!(next.args, want, "{permissions:?}");
            assert_eq!(next.stdin.as_deref(), Some("and the tests"));
        }
    }

    /// An option of `args` that takes several values (`-i`/`--image`, `1..`
    /// in 0.164.0's parser) ends at `--skip-git-repo-check`, so it never
    /// takes `resume <id> -` or the `-` of the prompt as files. Checked
    /// against the 0.164.0 binary's parser: `-i x.png resume <id> --help`
    /// printed `codex exec`'s help (the subcommand was taken as images),
    /// `-i x.png --skip-git-repo-check resume <id> --help` the help of
    /// `codex exec resume`, and `-i x.png --skip-git-repo-check - extra`
    /// refused `extra` (the `-` was the prompt).
    #[test]
    fn a_greedy_option_in_args_cannot_take_the_resume() {
        let mut a = agent(Permissions::Auto);
        a.args = strings(&["-i", "shot.png"]);
        for (thread, tail) in [
            (None, &["-i", "shot.png", "--skip-git-repo-check", "-"][..]),
            (
                Some(THREAD),
                &[
                    "-i",
                    "shot.png",
                    "--skip-git-repo-check",
                    "resume",
                    THREAD,
                    "-",
                ][..],
            ),
        ] {
            let args = turn_args(&a, thread);
            assert!(args.ends_with(&strings(tail)), "{args:?}");
            // Once: the flag may not be repeated.
            assert_eq!(
                args.iter()
                    .filter(|x| *x == "--skip-git-repo-check")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn only_a_thread_id_is_resumed() {
        assert!(is_thread_id(THREAD));
        assert!(is_thread_id("01A17400-230F-79E2-877D-E271E75B56D9"));
        for id in [
            "",
            "whisrs-cx",
            "ses_e90ef9da1ffe00000000000000",
            "01a17400-230f-79e2-877d-e271e75b56d",
            "01a17400-230f-79e2-877d-e271e75b56d9a",
            "01a17400x230f-79e2-877d-e271e75b56d9",
            "01a17400-230f-79e2-877d-e271e75b56dz",
        ] {
            assert!(!is_thread_id(id), "{id}");
        }
        // A stored id that is not one fails the turn: Codex would look it up
        // as a thread name, and could find a session whisrs did not start.
        let err = Codex
            .launch(
                &agent(Permissions::Auto),
                &resumed("my-named-thread"),
                &request("x"),
            )
            .unwrap_err();
        assert!(err.contains("is not a Codex thread id") && err.contains("whisrs agent cx --new"));
        assert!(!err.contains('\u{2014}'));
    }

    #[test]
    fn the_instruction_is_a_toml_string_that_reads_back_unchanged() {
        assert_eq!(instruction_config("  "), None);
        assert_eq!(
            instruction_config(" Be brief. ").as_deref(),
            Some("developer_instructions=\"Be brief.\"")
        );
        // What Codex does with `-c`: the value parsed as TOML.
        for text in [
            "Say \"done\" with a \\ backslash.",
            "line one\nline two\ttabbed\r\n",
            "true",
            "[1, 2]",
            "'''single''' and \"\"\"triple\"\"\"",
            "bell \u{7} escape \u{1b} del \u{7f} next-line \u{85}",
            "Unicode: café, 日本語, emoji 🎙",
            crate::agent::DEFAULT_REPLY_INSTRUCTION,
        ] {
            let config = format!("developer_instructions={}", toml_string(text));
            let parsed: toml::Table = toml::from_str(&config).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(
                parsed["developer_instructions"].as_str(),
                Some(text),
                "{config}"
            );
        }
        // A blank instruction adds no `-c` of its own.
        let mut quiet = agent(Permissions::Auto);
        quiet.instruction = " ".into();
        let args = turn_args(&quiet, None);
        assert_eq!(args.iter().filter(|a| *a == "-c").count(), 1);
        assert!(!args.iter().any(|a| a.starts_with("developer_instructions")));
    }

    #[test]
    fn only_a_parent_sessions_variables_and_the_log_filter_are_stripped() {
        for name in PARENT_SESSION_VARS.iter().chain(DAEMON_VARS) {
            assert!(Codex.strips_env(name), "{name}");
        }
        for name in [
            "CODEX_HOME",
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "PATH",
            "HOME",
        ] {
            assert!(!Codex.strips_env(name), "{name}");
        }
    }

    /// Run a fixture through the parser and the base's state, as a turn
    /// would: the updates sent and the state at the end.
    fn replay(fixture: &str, session: Option<&str>) -> (Vec<TurnUpdate>, StreamState) {
        let (updates, mut rx) = TurnUpdates::channel();
        let mut state = StreamState::new(session.map(String::from));
        for line in fixture.lines() {
            for event in parse_line(line, false) {
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
    fn an_acknowledgement_then_a_command_is_an_interim_and_the_last_message_the_reply() {
        let lines: Vec<&str> = ACK_THEN_TOOL.lines().collect();
        assert_eq!(
            parse_line(lines[0], false),
            [StreamEvent::Session(THREAD.into())]
        );
        // The mock's warning item and turn.started say nothing.
        assert!(parse_line(lines[1], false).is_empty());
        assert!(parse_line(lines[2], false).is_empty());
        assert_eq!(
            parse_line(lines[3], false),
            [StreamEvent::Text {
                message: "item_1".into(),
                text: "<spoken>On it.</spoken> Running that now.".into(),
            }]
        );
        assert_eq!(
            parse_line(lines[4], false),
            [StreamEvent::Tool("command_execution".into())]
        );
        // The command's completion and turn.completed say nothing.
        assert!(parse_line(lines[5], false).is_empty());
        assert!(parse_line(lines[7], false).is_empty());
        let (sent, state) = replay(ACK_THEN_TOOL, None);
        assert_eq!(
            sent,
            [
                TurnUpdate::Session(resumed(THREAD)),
                TurnUpdate::Activity("Running a command".into()),
                TurnUpdate::Interim("<spoken>On it.</spoken> Running that now.".into()),
            ]
        );
        let reply = state.reply().unwrap();
        assert!(
            reply.starts_with("Tool output was:")
                && reply.ends_with("<spoken>The command ran.</spoken>"),
            "{reply}"
        );
        assert_eq!(spoken::spoken_reply(&reply), "The command ran.");
        assert_eq!(
            outcome(&agent(Permissions::Auto), &Codex, &state, &[], &status(0)),
            TurnOutcome::Reply(reply)
        );
    }

    #[test]
    fn a_resumed_turn_names_its_own_thread_and_replies() {
        let (sent, state) = replay(RESUMED_REPLY, Some(THREAD));
        // Its own thread: nothing to report, no tool, no interim.
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(
            state.reply().as_deref(),
            Some("pong 2 <spoken>Pong number 2.</spoken>")
        );
    }

    #[test]
    fn file_changes_and_web_searches_are_activities() {
        let (sent, state) = replay(FILE_CHANGE, None);
        assert_eq!(
            sent[1..],
            [
                TurnUpdate::Activity("Editing files".into()),
                TurnUpdate::Interim("<spoken>Writing it.</spoken>".into()),
            ]
        );
        assert!(state
            .reply()
            .unwrap()
            .ends_with("<spoken>The command ran.</spoken>"));
        let (sent, state) = replay(WEB_SEARCH, None);
        assert_eq!(
            sent[1..],
            [TurnUpdate::Activity("Searching the web".into())]
        );
        assert_eq!(
            state.reply().as_deref(),
            Some("Searched. <spoken>I looked it up.</spoken>")
        );
    }

    /// Item shapes the mock cannot make, as `exec_events.rs` of 0.164.0
    /// serializes them.
    #[test]
    fn mcp_calls_plans_and_helpers_are_tools() {
        assert_eq!(
            parse_line(
                r#"{"type":"item.started","item":{"id":"item_3","type":"mcp_tool_call","server":"whisrs","tool":"list_recordings","arguments":{},"result":null,"error":null,"status":"in_progress"}}"#,
                false
            ),
            [StreamEvent::Tool("mcp__whisrs__list_recordings".into())]
        );
        assert_eq!(
            parse_line(
                r#"{"type":"item.started","item":{"id":"item_4","type":"todo_list","items":[{"text":"Read the code","completed":false}]}}"#,
                false
            ),
            [StreamEvent::Tool("todo_list".into())]
        );
        assert_eq!(
            parse_line(
                r#"{"type":"item.started","item":{"id":"item_5","type":"collab_tool_call","tool":"spawn_agent","sender_thread_id":"t","receiver_thread_ids":[],"prompt":null,"agents_states":{},"status":"in_progress"}}"#,
                false
            ),
            [StreamEvent::Tool("collab_tool_call".into())]
        );
    }

    #[test]
    fn a_failed_turn_fails_with_its_message() {
        let a = agent(Permissions::Auto);
        let (sent, state) = replay(TURN_FAILED, None);
        // The thread was started before the model failed: kept, and it
        // resumes (measured).
        assert_eq!(
            sent,
            [TurnUpdate::Session(resumed(
                "01a17400-3f5e-7711-acfd-6693d0d64e53"
            ))]
        );
        let TurnOutcome::Failed(message) = outcome(&a, &Codex, &state, &[], &status(1)) else {
            panic!("turn.failed fails the turn");
        };
        assert_eq!(
            message,
            "Agent 'cx': Codex failed: mock: this request is refused."
        );
        assert_eq!(
            spoken::spoken_error(&message),
            "Codex failed: mock: this request is refused."
        );
    }

    #[test]
    fn no_sign_in_fails_once_with_what_to_do() {
        let (_, state) = replay(NOT_LOGGED_IN, None);
        let TurnOutcome::Failed(message) =
            outcome(&agent(Permissions::Auto), &Codex, &state, &[], &status(1))
        else {
            panic!("turn.failed fails the turn");
        };
        assert_eq!(
            message,
            "Agent 'cx': Codex failed: it is not signed in, or its sign-in expired (401 \
             Unauthorized). Run `codex login` in a terminal, then try again."
        );
        assert_eq!(
            spoken::spoken_error(&message),
            "Codex failed: it is not signed in, or its sign-in expired (401 Unauthorized)."
        );
        assert!(!message.contains('\u{2014}'));
        // The retries before it are only warnings.
        for line in NOT_LOGGED_IN.lines().filter(|l| !l.contains("turn.failed")) {
            assert!(
                parse_line(line, false)
                    .iter()
                    .all(|e| !matches!(e, StreamEvent::Error(_))),
                "{line}"
            );
        }
        assert_eq!(failure("", false), "the turn failed with no message");
        assert_eq!(failure("  plain words ", true), "plain words");
    }

    /// A key in `CODEX_API_KEY` comes before the sign-in, so a 401 with one
    /// is that key, not a missing `codex login`.
    #[test]
    fn a_refused_api_key_is_not_a_missing_sign_in() {
        let body = "unexpected status 401 Unauthorized: Missing bearer or basic authentication";
        let keyed = failure(body, true);
        assert_eq!(
            keyed,
            "the API key in CODEX_API_KEY was refused (401 Unauthorized). Fix it in whisrsd's \
             environment, or remove it there to use the sign-in of `codex login`"
        );
        assert!(failure(body, false).contains("Run `codex login`"));
        let line = NOT_LOGGED_IN
            .lines()
            .find(|l| l.contains("turn.failed"))
            .unwrap();
        let [StreamEvent::Error(message)] = &parse_line(line, true)[..] else {
            panic!("turn.failed fails the turn");
        };
        assert!(
            message.starts_with("the API key in CODEX_API_KEY was refused"),
            "{message}"
        );
        assert!(!keyed.contains('\u{2014}'));
    }

    /// An empty final message is the turn's last message: no reply, never
    /// the acknowledgement before the tool call (spoken already).
    #[test]
    fn an_empty_last_message_is_no_reply_not_the_acknowledgement() {
        let empty_last = format!(
            "{}\n{}\n",
            ACK_THEN_TOOL
                .lines()
                .filter(|l| !(l.contains("agent_message") && l.contains("Tool output was")))
                .filter(|l| !l.contains("turn.completed"))
                .collect::<Vec<_>>()
                .join("\n"),
            r#"{"type":"item.completed","item":{"id":"item_9","type":"agent_message","text":""}}"#
        );
        let (sent, state) = replay(&empty_last, None);
        assert_eq!(
            sent[1..],
            [
                TurnUpdate::Activity("Running a command".into()),
                TurnUpdate::Interim("<spoken>On it.</spoken> Running that now.".into()),
            ]
        );
        assert_eq!(state.reply(), None);
        assert_eq!(
            outcome(&agent(Permissions::Auto), &Codex, &state, &[], &status(0)),
            TurnOutcome::Failed("Agent 'cx': Codex ended the turn without a reply.".into())
        );
        assert_eq!(
            parse_line(
                r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message"}}"#,
                false
            ),
            [StreamEvent::Text {
                message: "item_1".into(),
                text: String::new()
            }]
        );
    }

    #[test]
    fn a_thread_codex_does_not_have_says_to_start_a_new_session() {
        let a = agent(Permissions::Auto);
        let stderr: Vec<String> = RESUME_UNKNOWN.lines().map(String::from).collect();
        let empty = StreamState::new(Some("01a17400-0000-7000-8000-000000000000".into()));
        let TurnOutcome::Failed(message) = outcome(&a, &Codex, &empty, &stderr, &status(1)) else {
            panic!("a failed resume fails the turn");
        };
        assert_eq!(
            message,
            "Agent 'cx': Codex no longer has this agent's session, so it cannot continue it. \
             `whisrs agent cx --new` starts a new one."
        );
        // Another failure: Codex's own `Error:` line, not a log line before it.
        let stderr = vec![
            "2026-10-25T16:54:40.671128Z ERROR codex_core::session: failed to record rollout \
             items"
                .to_string(),
            "Error: failed to initialize in-process app-server client: boom".to_string(),
        ];
        assert_eq!(
            Codex.stderr_failure(&a, &stderr).as_deref(),
            Some(
                "Agent 'cx': Codex failed: failed to initialize in-process app-server client: \
                 boom."
            )
        );
        // Nothing it knows: the base's generic line.
        let clap = vec!["error: unexpected argument '--sandbox' found".to_string()];
        assert_eq!(Codex.stderr_failure(&a, &clap), None);
        let TurnOutcome::Failed(message) = outcome(&a, &Codex, &empty, &clap, &status(2)) else {
            panic!("a usage error fails the turn");
        };
        assert_eq!(
            message,
            "Agent 'cx': Codex failed: error: unexpected argument '--sandbox' found."
        );
    }

    #[test]
    fn anything_else_is_nothing() {
        for line in [
            "",
            "not json",
            "[1, 2]",
            r#"{"type":"turn.started"}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1}}"#,
            r#"{"type":"error","message":"Reconnecting... 1/5"}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"warning"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"reasoning","text":"hmm"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"no id"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"ls","aggregated_output":"","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_4","type":"todo_list","items":[]}}"#,
            r#"{"type":"item.updated","item":{"id":"item_4","type":"todo_list","items":[]}}"#,
            r#"{"type":"item.started","item":{"id":"item_6","type":"reasoning","text":""}}"#,
            r#"{"type":"item.started","item":{"id":"item_7","type":"mcp_tool_call","tool":"x"}}"#,
            r#"{"type":"thread.started","thread_id":"my-named-thread"}"#,
        ] {
            assert!(parse_line(line, false).is_empty(), "{line}");
        }
        assert_eq!(
            parse_line(r#"{"type":"turn.failed","error":{}}"#, false),
            [StreamEvent::Error("the turn failed with no message".into())]
        );
    }
}
