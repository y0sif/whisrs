# Agent mode

Agent mode lets you talk to a coding agent by voice. Press an agent's hotkey (or run
`whisrs agent <name>`), say what you want, and press again: whisrs sends your words to a
Claude Code, OpenCode, Codex or GitHub Copilot session working in that agent's `dir`, and
speaks a short reply when the turn ends. The next press continues the same session, so you
can answer the agent's question or make the decision it needs.

## Setup

```toml
[tts]
enabled = true          # to hear the replies; without it they come as notifications only

[[agents]]
name = "jarvis"
hotkey = "Super+Shift+J"
new_session_hotkey = "Super+Shift+N"   # optional: a voice turn in a fresh session
dir = "~/code/site"

[[agents]]
name = "coder"
harness = "opencode"
hotkey = "Super+Shift+K"
dir = "~/code/api"
```

Each entry is one agent working in one directory; run `whisrs restart` after a change. The keys:

- `name` (required): lowercase letters, digits, `-` and `_`; `whisrs agent <name>` uses it.
- `dir` (required): the project the agent works in. It must exist.
- `harness`: `claude` (the default), `opencode`, `codex` or `copilot`. Each has its own
  section below with what it needs.
- `hotkey` and `new_session_hotkey`: optional, with the [hotkey syntax](configuration.md#hotkeys).
- `permissions`: what the agent may do without asking, `auto` (default), `bypass` or `ask`.
  Each harness maps them its own way: see its section.
- `command`: the harness binary, when the daemon's `PATH` lacks it (a systemd user service
  often has no `~/.local/bin`). The harness gets the daemon's environment, so set its own
  variables (`CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_USE_BEDROCK`, `COPILOT_PROVIDER_*`) there.
- `args`: extra arguments for the harness. `reply_instruction` replaces the built-in voice
  instruction. `feedback` sets what the agent says while it works.

Every key, with its default, is in the [config reference](config-reference.md#agents).

```
whisrs agent                       # list the agents: harness, idle | working | needs-you, session id
whisrs agent jarvis                # record a voice turn (same as the hotkey; press again to stop)
whisrs agent jarvis --text "..."   # send a typed turn (`--text -` reads stdin)
whisrs agent jarvis --new          # a voice turn in a fresh session (with --text, a typed one)
whisrs agent jarvis --reset        # forget the session and send nothing
whisrs agent jarvis --stop         # stop the running turn and keep the session
```

## How a turn goes

- The instruction is recorded like command mode's, with its cues and silence auto-stop, on
  any transcription backend. Dictation is free again as soon as the transcript is ready.
- The reply is spoken when whisrs is idle: one that arrives while you dictate waits for the
  dictation, and one that arrives during a `whisrs recording` waits for it to stop. Several
  replies queue up. With `[general] notify` on, each also shows as a notification.
- An instruction with nothing in it is not sent ("I didn't catch that."), and a failed turn
  is spoken in a short sentence, with the full message as a notification.
- The agent's hotkey while its reply plays stops the reply and starts recording, so
  answering is one press. A dictation, `whisrs command` or an `[[llm_commands]]` hotkey also
  stops an agent reply. `whisrs cancel` while a reply plays drops the ones queued after it.
- A press while that agent's turn runs is refused ("Still working on the last one."). Other
  agents can work at the same time.
- `--new` (or `new_session_hotkey`) sends the turn to a fresh session; the old one stays in
  the harness, and whisrs forgets it only once there is something to send. `--new` and
  `--reset` are refused while a turn of that agent runs: stop it first with `--stop`.

The spoken reply: whisrs asks the agent to end every final message with a short reply in
`<spoken></spoken>` tags, written to be heard. It takes the last `<spoken>` block, or the first
two sentences when there is none, drops code blocks, inline code over three words, URLs, paths
and markdown (a link keeps its text), and caps it at 400 characters. When nothing speakable is
left, it says "Done."

## Sessions

whisrs keeps each agent's session id in `~/.local/share/whisrs/agents.json`, and only ever
continues, stops or reads the sessions it started. `whisrs agent` lists each session's id; open
the session with it in the harness itself (each harness below says how). Sessions outlive the
daemon: a turn still running when `whisrsd` restarts is watched again with Claude Code, and
ended with the other harnesses. A process an OpenCode, Codex or Copilot turn left running (a dev
server started with `&`) outlives a stopped `whisrs.service` and is stopped when `whisrsd` next
starts.

Changing an agent's `harness` or `dir` starts a new session. After `whisrs restart`, another
changed key applies from the next turn, or, where the session fixes it, from the next `--new`:

| Key | Claude Code | OpenCode | Codex | Copilot |
|---|---|---|---|---|
| `permissions` | session start | every turn | every turn | every turn |
| `args` | session start | session start | every turn | every turn |
| `reply_instruction`, `feedback` | session start | session start | session start | every turn |

A lower `feedback` level applies from the restart, since whisrs filters what it speaks.

## Feedback levels

`feedback` sets what an agent says while it works, before its reply:

| `feedback` | What you hear |
|---|---|
| `summary` | the reply only |
| `brief` (default) | a short acknowledgement when the agent starts using tools ("On it.", "Let me check."), then the reply |
| `conversational` | the acknowledgement, a one-sentence update at a real milestone, at least 20 seconds apart and never the same twice, then the reply |

The agent writes these lines itself, on whisrs's request, so they depend on the model
following it. They wait like a reply, are dropped when they waited too long or the reply
arrives, and never show as notifications. An unknown value warns at startup and means
`brief`.

## The agent overlay

With an `[[agents]]` entry, whisrs shows a row for each agent at work in a screen corner
(bottom right by default), apart from the dictation overlay: a shape (`style = "orb"` or
`"face"`) beside a card with the agent's name, state, activity and time. `[agent_overlay]
enabled` follows `[general] overlay` unless set.

- States: listening, transcribing, working, speaking, needs you (it waits for a permission
  prompt), done and error. The activity line names what it does ("Running a command",
  "Editing files", "Reading your recordings"), and a finished row says "Finished in 1:24".
- Up to four rows show; with more agents at work, the fourth says "+N more". A done row
  fades out a few seconds after its reply played.
- It draws on Wayland compositors with layer-shell (Hyprland, Sway) and on X11, where it
  scales by `Xft.dpi`. A fractional Wayland scale draws at the next whole one.
- GNOME has no agent overlay yet. There, and whenever the overlay is off, notifications
  ("Speak to 'jarvis'...", "Sending to 'jarvis'...") stand in for it.

## Claude Code

`harness = "claude"` (the default) drives `claude --bg` background sessions. You need:

- Claude Code 2.1.257 or later (`claude`), logged in.
- The agent's `dir` trusted in Claude Code: open it with `claude` once and accept the trust
  prompt. whisrs says so when a session refuses the directory.

| `permissions` | Claude Code |
|---|---|
| `auto` (default) | `--permission-mode auto` |
| `bypass` | `--permission-mode bypassPermissions` |
| `ask` | `--permission-mode default`: asks before acting, even when your own default is `auto` |

- Each agent's session is named `whisrs-<name>` and shows in `claude agents`;
  `claude attach <id>` opens it, with the id `whisrs agent` shows.
- When the agent waits for a permission prompt, `whisrs agent` shows needs-you and whisrs
  says so ("Waiting for your approval to use Bash."). Approve it in `claude attach <id>`;
  the next turn you send replaces the waiting one.
- A session can work on its own: when a command it started in the background ends, Claude
  Code goes on, and that reply is spoken too. While it works, a new turn is refused ("It's
  still running a background task."), so a long-running command (a dev server) keeps
  refusing turns. `whisrs agent <name> --stop` ends the work; then send again.
- Your words are on the `claude --bg` command line, which other processes can read (`ps`)
  until it returns; the other harnesses read them on stdin.
- `args = ["--remote-control", "jarvis"]` lets you follow and answer the session from the
  Claude app.
- Restarting or stopping `whisrs.service` leaves Claude Code's background service and its
  sessions running. When that service is not running yet, the first `claude --bg` whisrs
  runs starts it with the daemon's environment, which it keeps for every background session
  it hosts, your own included, until it exits.

## OpenCode

`harness = "opencode"` runs one `opencode run` per turn. You need OpenCode 2 (`opencode`;
OpenCode 1 has no `run --standalone`) with a model it can use from `opencode run`, from a
provider you set up with `opencode auth`. Each turn starts a private OpenCode server, which
takes a second or two.

| `permissions` | OpenCode |
|---|---|
| `auto` (default) | your own OpenCode rules (OpenCode allows most tools unless you set rules) |
| `bypass` | `--auto`: approves every permission request; rules that deny still apply |
| `ask` | asks before shell commands and file changes, so they are refused: reading and searching work |

- Nobody can answer a permission question during a turn: what would ask is refused, and
  the reply usually says what the agent could not do. An OpenCode agent never shows
  needs-you.
- With `ask`, an approval you saved in OpenCode with "always" still wins, so that command
  runs.
- `opencode --session <id>` opens the session; it is titled `whisrs-<name>`. A model or agent
  in `args` (`["-m", "anthropic/claude-sonnet-4-5"]`, `["--agent", "plan"]`) stays the
  session's.
- The voice instruction opens the session's first message, so it shows there in OpenCode,
  and a compaction of a long session may summarize it away (start a fresh one with `--new`).
- `--stop` and a daemon restart interrupt the turn; the session keeps what it did, and the
  next turn continues it.

## Codex

`harness = "codex"` runs one `codex exec` per turn. You need the Codex CLI (`codex`),
signed in with `codex login` (a ChatGPT plan, or an API key at API prices). A
`CODEX_API_KEY` in the daemon's environment wins over the sign-in. When Codex is not signed
in, the turn fails after about 40 seconds with "it is not signed in, or its sign-in
expired".

| `permissions` | Codex |
|---|---|
| `auto` (default) | `--approve-for-me`: the workspace-write sandbox, so the agent changes files in `dir` (no network); a command that needs more goes to Codex's automatic reviewer, one more model call |
| `bypass` | `--dangerously-bypass-approvals-and-sandbox`: no sandbox and no approvals |
| `ask` | `--sandbox read-only`: the agent reads files and runs commands that only read |

- Nobody can answer an approval request during a turn: what Codex may not do is refused,
  and a Codex agent never shows needs-you. Leave sandbox and approval flags, and
  `--skip-git-repo-check` (whisrs passes it, so `dir` can be any folder), out of `args`.
- With `auto` or `bypass` in a git repository, Codex records the repository as trusted in
  its own `config.toml`, as running it there yourself does, and then loads the project's
  `.codex/` settings.
- whisrs turns Codex's usage analytics off for its runs, which would hold every reply
  back by seconds; `args = ["-c", "analytics.enabled=true"]` turns them back on.
- The voice instruction goes in as Codex's developer instructions on every turn, and
  replaces any `developer_instructions` of your own: put your text in `reply_instruction`.
- `codex resume <id>` opens the session. When Codex no longer has it, the turn fails and
  `whisrs agent <name> --new` starts a new one.
- `--stop` interrupts the turn. Stopping a command in the `auto` sandbox can leave empty
  `.git`, `.codex`, `.agents` and `.aws` directories in `dir`; delete them if they are in
  the way.

## GitHub Copilot

`harness = "copilot"` runs one `copilot` per turn in its prompt mode. You need the GitHub
Copilot CLI (`copilot`, not `gh copilot`), signed in with `copilot login` or a token in
`COPILOT_GITHUB_TOKEN`, `GH_TOKEN` or `GITHUB_TOKEN`, or set up with a model provider of
your own. A token in the daemon's environment wins over `copilot login`, so a `GITHUB_TOKEN`
meant for something else replaces your sign-in. With a GitHub account, every turn uses your
Copilot plan's usage as the same prompt typed into Copilot would (`copilot help billing`).

| `permissions` | Copilot |
|---|---|
| `auto` (default) | `--allow-all-tools`: commands and file changes in `dir` run; a file outside it, or a URL, needs an approval, so it is refused |
| `bypass` | `--yolo`: every tool, path and URL allowed |
| `ask` | `--deny-tool=shell --deny-tool=write`: the agent reads and searches files; commands and file changes are refused, also when your Copilot settings allow them |

- Nobody can answer an approval request during a turn: it is refused, and a Copilot agent
  never shows needs-you. A tool from an MCP server keeps Copilot's own rules, and `ask`
  does not cover it, so an approval you saved in Copilot may let one run.
- Leave out of `args`: `-p` (whisrs sends the prompt on stdin), `--output-format`,
  `--session-id` and `--resume` (whisrs passes them), and `--name` and `--trust-folder`
  (Copilot refuses them beside those).
- A turn that starts with `/` runs a Copilot command or skill, as if typed into Copilot.
- The voice instruction reaches Copilot as a custom instructions file of whisrs's own on
  every turn, so it never enters the session's history; `--no-custom-instructions` in
  `args` turns it off, with your own instruction files.
- `copilot --resume=<id>` opens the session. When Copilot no longer has it, the turn fails
  and `whisrs agent <name> --new` starts a new one.
- `--stop` and a daemon restart interrupt the turn; the session keeps what it did.
