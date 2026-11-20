//! The OpenCode driver (OpenCode 2, `opencode run`): one process per turn,
//! on the process-per-turn base ([`super::process`]). Verified on 2.0.29.
//!
//! - **Every turn:** `opencode run --standalone --format json --session
//!   <id> [--title whisrs-<agent> args…] [--auto]`, the prompt on stdin
//!   (argv words with spaces reach the model quoted). `--standalone` runs a
//!   private server as the run's child instead of OpenCode's shared
//!   background service, which would outlive the turn, ignore the run's
//!   environment (`OPENCODE_CONFIG_CONTENT` included) and keep a turn going
//!   when its `run` is killed. whisrs mints the session id in OpenCode's own
//!   layout ([`new_session_id`]): `--session` creates a session it does not
//!   find, so the first turn creates it and every later one continues it
//!   with the same command. The id is the agent's as soon as the first
//!   turn's process runs ([`Launch::session`]), so a turn stopped or cut off
//!   before its first line keeps it, but pending ([`SessionRef::pending`])
//!   until a text or a tool call shows the agent working in the session.
//!   `--title`, `args` and the instruction are passed on the first turn,
//!   and on every turn while the session is pending: `--title` names the
//!   session (and saves the model call that would title it), a model or
//!   agent chosen with `args` stays the session's, and OpenCode keeps the
//!   title of a session it has and takes the rest again (measured on
//!   2.0.29: a first turn that failed on `-m nosuch/model`, then the same
//!   id with `-m mock/mock-1`, ran on the new model in the same session).
//!   So a first turn that only failed (a typo in `args`) does not pin the
//!   session to it, and one stopped before OpenCode made the session does
//!   not leave a session without the instruction.
//! - **The voice instruction** opens the first turn's prompt. `run` has no
//!   system prompt option; an agent's `system` (through
//!   `OPENCODE_CONFIG_CONTENT`) replaces OpenCode's own prompt, and the
//!   session then fails to resume without that agent; config
//!   `instructions` did not reach the model (2.0.29, measured). In the
//!   session's history it holds for every later turn (measured); it shows in
//!   the session's first message, and a compaction of a long session may
//!   summarize it.
//! - **Permissions** ([`permission_args`], [`ask_config`]): `auto` passes
//!   nothing (the user's rules; OpenCode allows most tools by default),
//!   `bypass` passes `--auto` (every ask approved, deny rules still hold),
//!   and `ask` adds rules that ask for shell commands and file changes to
//!   `OPENCODE_CONFIG_CONTENT`, after the user's config rules (the last
//!   match wins). Nobody can approve in `run`: an ask is rejected with
//!   feedback to the model, which goes on without it, so the turn never
//!   waits for you. OpenCode's own saved approvals ("always" answers) and a
//!   session's allows come after every config rule, so they win over these:
//!   `ask` refuses commands and edits unless OpenCode already has an
//!   approval saved for them.
//! - **Output:** one JSON object per line, `{type, timestamp, sessionID,
//!   part | error}`. `text` is a finished text part (`part.messageID`,
//!   `part.text`), `tool_use` a finished tool call (`part.tool`), `error` a
//!   failed turn; `step_start` and `step_finish` say nothing whisrs needs.
//!   A tool call is printed when it ends, so its activity, and the
//!   acknowledgement before it, come then. Texts a fast turn did not print
//!   live come after the turn, in order (the run's reconcile). No line ends
//!   the turn: the process's exit does.
//! - **Stop:** SIGINT interrupts the session and exits 130; the private
//!   server ends with the run (measured), and the session continues on the
//!   next turn, the stopped tool call in its history.

use std::sync::atomic::{AtomicU16, Ordering};

use serde_json::Value;

use super::process::{random_bytes, Launch, StreamEvent, TurnHarness};
use super::{Agent, Permissions, SessionRef, TurnRequest};

/// What a parent OpenCode session sets in the environment of the
/// processes it runs (its tool shells), and the server passwords, which
/// belong to one server. Removed from the `opencode` child, besides what
/// every driver removes.
pub const PARENT_SESSION_VARS: &[&str] = &[
    "OPENCODE",
    "OPENCODE_SESSION_ID",
    "OPENCODE_TERMINAL",
    "AGENT",
    "OPENCODE_PASSWORD",
    "OPENCODE_SERVER_PASSWORD",
];

/// The environment variable OpenCode reads extra config from (a JSON
/// object, merged after the config files).
pub const CONFIG_CONTENT_VAR: &str = "OPENCODE_CONFIG_CONTENT";

/// The rules `permissions = "ask"` adds: shell commands and file changes
/// (edit, write and patch all ask as `edit`) ask, so a headless run rejects
/// them, unless OpenCode has an approval of its own saved for them ("always"
/// answers, a session's allows), which wins over config rules. Reads and
/// searches keep the user's rules.
const ASK_RULES: [(&str, &str); 2] = [("shell", "ask"), ("edit", "ask")];

/// The OpenCode harness.
pub struct OpenCode;

impl TurnHarness for OpenCode {
    fn label(&self) -> &'static str {
        "OpenCode"
    }

    fn launch(
        &self,
        agent: &Agent,
        session: &SessionRef,
        request: &TurnRequest,
    ) -> Result<Launch, String> {
        // A pending session may not exist in OpenCode yet: started over.
        let first = session.session_id.is_none() || session.pending;
        let session_id = session.session_id.clone().unwrap_or_else(new_session_id);
        let mut env = Vec::new();
        if agent.permissions == Permissions::Ask {
            let existing = std::env::var(CONFIG_CONTENT_VAR).ok();
            let config = ask_config(existing.as_deref()).map_err(|e| {
                format!(
                    "Agent '{}': permissions = \"ask\" adds rules to {CONFIG_CONTENT_VAR}, but \
                     {e}. Fix it there, or use another permissions value.",
                    agent.name
                )
            })?;
            env.push((CONFIG_CONTENT_VAR.to_string(), config));
        }
        let prompt = if first {
            first_prompt(&agent.instruction, &request.text)
        } else {
            request.text.clone()
        };
        Ok(Launch {
            args: turn_args(agent, &session_id, first),
            stdin: Some(prompt),
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

    /// Nothing to recognize: OpenCode reports its failures as `error`
    /// lines, and `--session` creates a session it does not find.
    fn stderr_failure(&self, _agent: &Agent, _stderr: &[String]) -> Option<String> {
        None
    }
}

/// The title of an agent's sessions in OpenCode.
pub fn session_title(agent: &Agent) -> String {
    format!("whisrs-{}", agent.name)
}

/// The arguments of a turn in `session_id`; `first` for a turn that may
/// create the session (the first, or one of a pending session).
pub fn turn_args(agent: &Agent, session_id: &str, first: bool) -> Vec<String> {
    let mut args: Vec<String> = ["run", "--standalone", "--format", "json", "--session"]
        .map(String::from)
        .to_vec();
    args.push(session_id.to_string());
    if first {
        args.push("--title".to_string());
        args.push(session_title(agent));
        args.extend(agent.args.iter().cloned());
    }
    args.extend(
        permission_args(agent.permissions)
            .iter()
            .map(|a| a.to_string()),
    );
    args
}

/// The flags of a harness-neutral setting, on every turn (`run` does not
/// keep them). `ask` is [`ask_config`] instead.
pub fn permission_args(permissions: Permissions) -> &'static [&'static str] {
    match permissions {
        Permissions::Bypass => &["--auto"],
        Permissions::Auto | Permissions::Ask => &[],
    }
}

/// `OPENCODE_CONFIG_CONTENT` for `permissions = "ask"`: the daemon's own
/// (`existing`, a JSON object) with [`ASK_RULES`] after its `permissions`,
/// or only them. An error says what is wrong with `existing`.
pub fn ask_config(existing: Option<&str>) -> Result<String, String> {
    let mut config = match existing.map(str::trim).filter(|c| !c.is_empty()) {
        None => serde_json::Map::new(),
        Some(content) => match serde_json::from_str::<Value>(content) {
            Ok(Value::Object(config)) => config,
            _ => return Err("the one whisrsd has is not a plain JSON object".to_string()),
        },
    };
    let rules = config
        .entry("permissions")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(rules) = rules else {
        return Err("its permissions is not a list".to_string());
    };
    for (action, effect) in ASK_RULES {
        rules.push(serde_json::json!({"action": action, "resource": "*", "effect": effect}));
    }
    Ok(Value::Object(config).to_string())
}

/// The first prompt of a session: the voice instruction, then what the user
/// said.
pub fn first_prompt(instruction: &str, text: &str) -> String {
    let instruction = instruction.trim();
    if instruction.is_empty() {
        return text.to_string();
    }
    format!("{instruction}\n\n{text}")
}

/// A new session id: `ses_`, then OpenCode's own layout (twelve hex digits
/// of the inverted creation time in milliseconds times 4096 plus a counter,
/// so newer sessions sort first, then fourteen random letters and digits).
pub fn new_session_id() -> String {
    static COUNTER: AtomicU16 = AtomicU16::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    session_id_at(
        ms,
        COUNTER.fetch_add(1, Ordering::Relaxed),
        random_bytes::<14>(),
    )
}

/// [`new_session_id`] at `ms` with `counter` and `random` given.
pub fn session_id_at(ms: u64, counter: u16, random: [u8; 14]) -> String {
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let value = !(ms.wrapping_mul(0x1000) + u64::from(counter & 0xfff));
    let mut id = String::from("ses_");
    for i in 0..6 {
        id.push_str(&format!("{:02x}", (value >> (40 - 8 * i)) & 0xff));
    }
    id.extend(
        random
            .iter()
            .map(|b| char::from(CHARS[usize::from(b % 62)])),
    );
    id
}

/// The events of one `opencode run --format json` line: the session it
/// names (an id starting `ses`; a failure before the session was resolved
/// names `""`), then a text part, a tool call or an error. Nothing for
/// `step_start`, `step_finish`, `reasoning`, an empty text and anything
/// that is not JSON.
pub fn parse_line(line: &str) -> Vec<StreamEvent> {
    let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    if let Some(id) = entry
        .get("sessionID")
        .and_then(Value::as_str)
        .filter(|id| id.starts_with("ses"))
    {
        events.push(StreamEvent::Session(id.to_string()));
    }
    let part = |key: &str| {
        entry
            .get("part")
            .and_then(|p| p.get(key))
            .and_then(Value::as_str)
    };
    match entry.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let (Some(message), Some(text)) = (
                part("messageID"),
                part("text").filter(|t| !t.trim().is_empty()),
            ) {
                events.push(StreamEvent::Text {
                    message: message.to_string(),
                    text: text.to_string(),
                });
            }
        }
        Some("tool_use") => {
            if let Some(tool) = part("tool") {
                events.push(StreamEvent::Tool(tool.to_string()));
            }
        }
        Some("error") => {
            let message = entry
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or("an error with no message");
            events.push(StreamEvent::Error(message.to_string()));
        }
        _ => {}
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::process::{outcome, Exit, StreamState};
    use crate::agent::progress::Feedback;
    use crate::agent::{spoken, Harness, TurnOutcome, TurnUpdate, TurnUpdates};
    use std::path::PathBuf;

    /// Real `opencode run --format json` output (2.0.29, the mock model of
    /// the e2e run): a text, a shell call, then the reply, all printed live.
    const ACK_THEN_TOOL: &str = include_str!("../../tests/fixtures/opencode/ack-then-tool.jsonl");
    /// A fast turn whose only text came after the turn (the reconcile's
    /// `_text-0` part).
    const REPLY_AFTER_THE_TURN: &str =
        include_str!("../../tests/fixtures/opencode/reply-after-the-turn.jsonl");
    /// A shell call an ask rule rejected, then the reply.
    const TOOL_REJECTED: &str = include_str!("../../tests/fixtures/opencode/tool-rejected.jsonl");
    /// `-m nosuch/model`: an error line, exit 1.
    const MODEL_UNAVAILABLE: &str =
        include_str!("../../tests/fixtures/opencode/model-unavailable.jsonl");
    /// `--session badid`: an error line naming no session, exit 1.
    const BAD_SESSION_ID: &str = include_str!("../../tests/fixtures/opencode/bad-session-id.jsonl");

    fn agent(permissions: Permissions) -> Agent {
        Agent {
            name: "coder".into(),
            hotkey: None,
            new_session_hotkey: None,
            harness: Harness::OpenCode,
            dir: PathBuf::from("/proj"),
            permissions,
            command: "opencode".into(),
            args: vec!["-m".into(), "anthropic/claude-sonnet".into()],
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

    #[test]
    fn permissions_map_to_opencode_flags() {
        // The first turn creates the session: title and args, once.
        assert_eq!(
            turn_args(&agent(Permissions::Auto), "ses_1", true),
            strings(&[
                "run",
                "--standalone",
                "--format",
                "json",
                "--session",
                "ses_1",
                "--title",
                "whisrs-coder",
                "-m",
                "anthropic/claude-sonnet",
            ])
        );
        // A later turn: the same command on the same id, permissions again.
        assert_eq!(
            turn_args(&agent(Permissions::Bypass), "ses_1", false),
            strings(&[
                "run",
                "--standalone",
                "--format",
                "json",
                "--session",
                "ses_1",
                "--auto"
            ])
        );
        assert_eq!(
            turn_args(&agent(Permissions::Bypass), "ses_1", true)
                .last()
                .map(String::as_str),
            Some("--auto")
        );
        assert_eq!(
            turn_args(&agent(Permissions::Ask), "ses_1", false),
            strings(&[
                "run",
                "--standalone",
                "--format",
                "json",
                "--session",
                "ses_1"
            ])
        );
    }

    #[test]
    fn the_first_turn_mints_a_session_and_opens_with_the_instruction() {
        let first = OpenCode
            .launch(
                &agent(Permissions::Auto),
                &SessionRef::default(),
                &request("fix the build"),
            )
            .unwrap();
        let id = &first.args[5];
        assert!(id.starts_with("ses_") && id.len() == 30, "{id}");
        // Named to the base, which makes it the agent's once the run starts.
        assert_eq!(first.session.as_ref(), Some(id));
        assert_eq!(
            first.stdin.as_deref(),
            Some("Answer by voice.\n\nfix the build")
        );
        assert!(first.env.is_empty());
        // A later turn continues it with the text alone.
        let next = OpenCode
            .launch(
                &agent(Permissions::Auto),
                &resumed(id),
                &request("and the tests"),
            )
            .unwrap();
        assert_eq!(&next.args[5], id);
        assert_eq!(next.stdin.as_deref(), Some("and the tests"));
        assert_eq!(next.session.as_ref(), Some(id));
        assert!(!next.args.contains(&"--title".to_string()));
        assert!(!next.args.contains(&"-m".to_string()));
        assert_eq!(first_prompt("  ", "hi"), "hi");
    }

    /// A pending session (its first turn only failed, or was stopped or cut
    /// off before the agent worked in it) is started over on the same id:
    /// the title, `args` and the instruction again.
    #[test]
    fn a_pending_session_is_started_over_on_the_same_id() {
        let a = agent(Permissions::Auto);
        let again = OpenCode
            .launch(&a, &pending("ses_1"), &request("fix the build"))
            .unwrap();
        assert_eq!(again.args, turn_args(&a, "ses_1", true));
        assert_eq!(again.session.as_deref(), Some("ses_1"));
        assert!(again.args.contains(&"--title".to_string()));
        assert!(again.args.contains(&"anthropic/claude-sonnet".to_string()));
        assert_eq!(
            again.stdin.as_deref(),
            Some("Answer by voice.\n\nfix the build")
        );
    }

    #[test]
    fn ask_adds_rules_after_the_users_own() {
        let parsed = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        let rules = serde_json::json!([
            {"action": "shell", "resource": "*", "effect": "ask"},
            {"action": "edit", "resource": "*", "effect": "ask"},
        ]);
        assert_eq!(
            parsed(&ask_config(None).unwrap()),
            serde_json::json!({ "permissions": rules })
        );
        assert_eq!(
            parsed(&ask_config(Some("  ")).unwrap())["permissions"],
            rules
        );
        let merged = parsed(
            &ask_config(Some(
                r#"{"model":"mock/mock-1","permissions":[{"action":"shell","resource":"*","effect":"allow"}]}"#,
            ))
            .unwrap(),
        );
        assert_eq!(merged["model"], "mock/mock-1");
        let list = merged["permissions"].as_array().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0]["effect"], "allow");
        assert_eq!(list[1], rules[0]);
        assert_eq!(list[2], rules[1]);
        for bad in ["{not json", "[1, 2]", r#""text""#] {
            assert!(ask_config(Some(bad))
                .unwrap_err()
                .contains("not a plain JSON object"));
        }
        assert!(ask_config(Some(r#"{"permissions":{"shell":"ask"}}"#))
            .unwrap_err()
            .contains("not a list"));
    }

    #[test]
    fn ask_puts_the_rules_in_the_child_environment() {
        let launch = OpenCode
            .launch(&agent(Permissions::Ask), &resumed("ses_1"), &request("x"))
            .unwrap();
        let (name, value) = &launch.env[0];
        assert_eq!(name, CONFIG_CONTENT_VAR);
        assert!(value.contains(r#""action":"shell""#), "{value}");
        assert!(!launch.args.contains(&"--auto".to_string()));
    }

    #[test]
    fn session_ids_have_opencodes_layout_newest_first() {
        let id = session_id_at(1_792_864_707_166, 1, [0; 14]);
        assert_eq!(id, "ses_e90ef9da1ffe00000000000000");
        let later = session_id_at(1_792_864_707_167, 1, [61; 14]);
        assert!(later < id, "a later millisecond sorts first");
        assert!(later.ends_with("zzzzzzzzzzzzzz"));
        // The counter keeps two ids of one millisecond apart, in order.
        assert!(session_id_at(5, 2, [0; 14]) < session_id_at(5, 1, [0; 14]));
        let (a, b) = (new_session_id(), new_session_id());
        assert_ne!(a, b);
        assert!(a.starts_with("ses_") && a.len() == 30);
        assert!(a[4..].chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn only_a_parent_sessions_variables_are_stripped() {
        for name in PARENT_SESSION_VARS {
            assert!(OpenCode.strips_env(name), "{name}");
        }
        for name in [
            CONFIG_CONTENT_VAR,
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_DIR",
            "XDG_DATA_HOME",
            "PATH",
        ] {
            assert!(!OpenCode.strips_env(name), "{name}");
        }
    }

    /// Run a fixture through the parser and the base's state, as a turn
    /// would: the updates sent and the state at the end.
    fn replay(fixture: &str, session: Option<&str>) -> (Vec<TurnUpdate>, StreamState) {
        replay_in(fixture, session.map(resumed).unwrap_or_default())
    }

    fn replay_in(fixture: &str, session: SessionRef) -> (Vec<TurnUpdate>, StreamState) {
        let (updates, mut rx) = TurnUpdates::channel();
        let mut state = StreamState::for_session(&session);
        for line in fixture.lines() {
            for event in OpenCode.events(line) {
                state.event(event, &updates);
            }
        }
        let mut sent = Vec::new();
        while let Ok(update) = rx.try_recv() {
            sent.push(update);
        }
        (sent, state)
    }

    fn session(id: &str) -> TurnUpdate {
        TurnUpdate::Session(resumed(id))
    }

    #[test]
    fn a_text_then_a_tool_call_is_an_interim_and_the_last_message_the_reply() {
        let lines: Vec<&str> = ACK_THEN_TOOL.lines().collect();
        assert_eq!(
            parse_line(lines[1]),
            [
                StreamEvent::Session("ses_whisrsfixtureacktool".into()),
                StreamEvent::Text {
                    message: "msg_173dc5ae1001U9hAcvGsNzL50a".into(),
                    text: "<spoken>On it.</spoken> Let me run that.".into(),
                },
            ]
        );
        assert_eq!(
            parse_line(lines[2]),
            [
                StreamEvent::Session("ses_whisrsfixtureacktool".into()),
                StreamEvent::Tool("shell".into()),
            ]
        );
        // step_start and step_finish only name the session.
        assert_eq!(
            parse_line(lines[0]),
            [StreamEvent::Session("ses_whisrsfixtureacktool".into())]
        );
        let (sent, state) = replay(ACK_THEN_TOOL, None);
        assert_eq!(
            sent,
            [
                session("ses_whisrsfixtureacktool"),
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
    }

    #[test]
    fn a_reply_printed_after_the_turn_is_the_reply_of_a_resumed_session() {
        let (sent, state) = replay(REPLY_AFTER_THE_TURN, Some("ses_whisrsfixturepong"));
        // Its own session: nothing to report, no tool, no interim.
        assert!(sent.is_empty(), "{sent:?}");
        let reply = state.reply().unwrap();
        assert_eq!(reply, "pong 1 <spoken>Pong number 1.</spoken>");
        assert_eq!(spoken::spoken_reply(&reply), "Pong number 1.");
        let a = agent(Permissions::Auto);
        let ok = Exit::Status(std::os::unix::process::ExitStatusExt::from_raw(0));
        assert_eq!(
            outcome(&a, &OpenCode, &state, &[], &ok),
            TurnOutcome::Reply(reply)
        );
    }

    #[test]
    fn a_rejected_tool_call_is_still_an_activity() {
        let (sent, state) = replay(TOOL_REJECTED, Some("ses_whisrsfixturerejected"));
        assert_eq!(sent, [TurnUpdate::Activity("Running a command".into())]);
        assert_eq!(
            state.reply().as_deref(),
            Some("Finished the tool step. <spoken>The tool step is done.</spoken>")
        );
    }

    #[test]
    fn an_error_line_fails_the_turn_with_its_message() {
        let failed = Exit::Status(std::os::unix::process::ExitStatusExt::from_raw(1 << 8));
        let a = agent(Permissions::Auto);
        // The first turn's id, reported pending as the run started: an error
        // line names it, but shows no agent working, so it stays pending and
        // the next turn passes `args` (the model a typo broke) again.
        let (sent, mut state) = replay_in(MODEL_UNAVAILABLE, pending("ses_whisrsfixturenoroute"));
        assert!(sent.is_empty(), "{sent:?}");
        // Still pending: a text would confirm it.
        let (updates, mut rx) = TurnUpdates::channel();
        state.event(
            StreamEvent::Text {
                message: "m".into(),
                text: "t".into(),
            },
            &updates,
        );
        assert_eq!(
            rx.try_recv().ok(),
            Some(session("ses_whisrsfixturenoroute"))
        );
        let next = OpenCode
            .launch(&a, &pending("ses_whisrsfixturenoroute"), &request("x"))
            .unwrap();
        assert!(next.args.contains(&"-m".to_string()));
        let TurnOutcome::Failed(message) = outcome(&a, &OpenCode, &state, &[], &failed) else {
            panic!("an error line fails the turn");
        };
        assert_eq!(
            message,
            "Agent 'coder': OpenCode failed: Model unavailable: nosuch/model."
        );
        assert_eq!(
            spoken::spoken_error(&message),
            "OpenCode failed: Model unavailable: nosuch/model."
        );
        // An error before any session names none: nothing to continue.
        let (sent, state) = replay(BAD_SESSION_ID, None);
        assert!(sent.is_empty(), "{sent:?}");
        let TurnOutcome::Failed(message) = outcome(&a, &OpenCode, &state, &[], &failed) else {
            panic!("an error line fails the turn");
        };
        assert!(
            message.contains("Expected a string starting with \"ses\" at [\"sessionID\"]."),
            "{message}"
        );
    }

    #[test]
    fn anything_else_is_nothing() {
        for line in [
            "",
            "not json",
            "[1, 2]",
            r#"{"type":"reasoning","sessionID":"","part":{"messageID":"m","text":"hmm"}}"#,
            r#"{"type":"text","sessionID":"","part":{"messageID":"m","text":"   "}}"#,
            r#"{"type":"text","sessionID":"","part":{"text":"no message id"}}"#,
            r#"{"type":"tool_use","sessionID":"","part":{"state":{}}}"#,
        ] {
            assert!(parse_line(line).is_empty(), "{line}");
        }
        assert_eq!(
            parse_line(r#"{"type":"error","sessionID":"ses_1","error":{}}"#),
            [
                StreamEvent::Session("ses_1".into()),
                StreamEvent::Error("an error with no message".into()),
            ]
        );
    }
}
