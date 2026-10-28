# whisrs — Project Conventions

## Environment

- OS: Arch Linux
- Shell: fish (no `&&` chaining — use `;` instead; no `export`, use `set -x`)
- Editor: nvim
- Python: managed with `uv` (not pip)
- Rust: managed with `cargo`

## Build Commands

```fish
cargo build                                    # compile debug binaries (includes all backends)
cargo clippy --all-targets -- -D warnings      # lint (strict, warnings = errors)
cargo test                                     # run all tests
cargo fmt                                      # format code
cargo fmt -- --check                           # check formatting (CI)
```

## Running

```fish
# First-time setup (interactive, no daemon needed)
whisrs setup

# Start the daemon
whisrsd &
# Or via systemd
systemctl --user enable --now whisrs.service

# Use the CLI (bind to a hotkey)
whisrs toggle    # start/stop dictation
whisrs cancel    # cancel and discard audio
whisrs speak     # read the selected text aloud via TTS (alias: read; press again to stop)
whisrs speak --text "..."   # speak the given text (--text - reads stdin); its own IPC command (speak-text)
whisrs agent     # list [[agents]]; `whisrs agent <name>` records a voice turn for that coding agent
                 # (same as its hotkey), --text "..." sends a typed one, --new records one (or sends
                 # --text) in a fresh session (same as new_session_hotkey), --reset forgets the session,
                 # --stop stops the running turn. The reply is spoken via [tts] when the turn ends
whisrs status    # query daemon state
whisrs restart   # restart the daemon (wraps systemctl --user when present)
whisrs command   # record a voice instruction, rewrite the selection via LLM
whisrs log       # dictation history
whisrs config    # interactive config editor
whisrs recording start [-t TITLE] [--shared-mic] [--num-speakers N] [--with "Sarah, Omar"] | stop | toggle | cancel | status   # record mic + system audio, transcribe after the stop
whisrs recording list | show <id|last> [-f md|json|txt] | path <id> | copy <id> | retry <id> [--with "..."]
whisrs recording name <id> <S2|"Speaker 2"|current name> <name>   # "" clears; rewrites the .md at once
whisrs recording summarize <id>   # [llm] summary (+ title if untitled) via the queue; [recording] summary = true does it after each transcript
                 # (every `whisrs recording` line: feature `recording`, in the default and both release builds)
whisrs mcp       # read-only MCP server on stdio over the recordings folder (feature `mcp`, needs `recording`; no daemon needed)

# Dev loop: rebuild this checkout and restart the daemon
./scripts/dev-install.sh             # build + install to ~/.cargo/bin + restart
./scripts/dev-install.sh --system    # build + sudo install to /usr/local/bin + restart

# Debug logging
set -x RUST_LOG debug; whisrsd
```

## Project Structure

Cargo workspace: the root package builds two binaries, plus five extracted crates.

```
src/
├── lib.rs                  # Crate root: WhisrsError + re-exports (config types, IPC, service ctl)
├── ipc.rs                  # IPC protocol: commands, responses, socket path, wire framing
├── service.rs              # Init abstraction (systemd/OpenRC): detect, restart, install hints
├── state.rs                # State machine (Idle → Recording → Transcribing → Idle;
│                           #   read-aloud: Idle → Synthesizing → Speaking → Idle)
├── history.rs              # Dictation history (whisrs log)
├── llm.rs                  # LLM calls for command mode
├── agent/                  # Agent mode ([[agents]]): voice turns to a coding agent, spoken replies
│   ├── mod.rs              #   AgentConfig + resolve_agents (validation), IPC types, Driver trait,
│   │                       #   SessionRef/TurnUpdate/TurnOutcome, the default voice instruction
│   ├── spoken.rs           #   <spoken> extraction + sanitize + 400-char cap (pure)
│   ├── claude.rs           #   Claude Code driver: claude --bg, stop + --bg --resume, hooks via --settings, poll
│   ├── claude_tail.rs      #   Transcript tail while a turn runs: tool activity + interims (pure parse_line)
│   ├── process.rs          #   Process-per-turn base: ProcessDriver<H: TurnHarness> (spawn in own pgid + own
│   │                       #   systemd scope + PDEATHSIG, JSON lines -> StreamEvent -> TurnUpdates, SIGINT/TERM/KILL
│   │                       #   stop of group + tree + scope, leftover-scope sweep at startup, outcome)
│   ├── opencode.rs         #   OpenCode driver (TurnHarness): opencode run --standalone --format json --session <id>
│   ├── codex.rs            #   Codex driver (TurnHarness): codex exec --json ... --skip-git-repo-check [resume <id>] -
│   ├── copilot.rs          #   Copilot driver (TurnHarness): copilot --output-format json --session-id=|--resume=<id>, prompt on stdin
│   ├── progress.rs         #   feedback levels + instruction deltas, activity labels, InterimTracker, ProgressGate
│   ├── display.rs          #   Agent overlay model: Board (voice > needs-you > working > done/error), rows, timer
│   └── store.rs            #   $XDG_DATA_HOME/whisrs/agents.json (0600, atomic), written only with agents
├── mcp.rs                  # whisrs mcp (feature mcp): read-only rmcp server, 4 tools over the recordings folder
├── replacements.rs         # [replacements]: whole-word fix-ups applied to dictated and recording text
├── cli/main.rs             # whisrs CLI (thin client, sends commands over socket)
├── daemon/main.rs          # whisrsd daemon (audio, transcription, typing, IPC server)
├── daemon/recording.rs     # Recording sessions, processing queue/worker, recovery, shutdown (feature recording)
├── daemon/agent.rs         # Agent runtime: slots, turns, reply FIFO (spoken when Idle), IPC handlers,
│                           #   hook events, the voice-turn recording sink
├── daemon/speak.rs         # Read-aloud: selection, speak --text, agent replies (speak_text)
├── recording/              # Recording feature (mic + system audio, transcribed after the stop)
│   ├── mod.rs              #   RecordingMeta (recording.json), segments, status types: every build (IPC wire);
│   │                       #   the submodules below are feature `recording`
│   ├── store.rs            #   Recordings folder: create, list (or list_headers: no transcript text), resolve ids, retention sweep
│   ├── recorder.rs         #   Per-track WAV writer (periodic header flushes), mic/system meters, live quality warnings (`LiveQuality`)
│   ├── wav_repair.rs       #   Fix WAV headers a crash left unfinalized
│   ├── echo.rs             #   Remove speaker echo from the mic track (system track as reference)
│   ├── speech.rs           #   Speech regions, condensed chunks + time maps, segments mapped back
│   ├── process.rs          #   Echo-cancel, send the speech, transcribe (cached, rate-limit retry), merge, write
│   ├── speakers.rs         #   Speaker separation: plan, align words to voices, ids by first appearance
│   ├── diarize.rs          #   Diarization sidecar client (contract, timeout, retries)
│   ├── naming.rs           #   Speaker names: user > self > --with > LLM > last voice > Speaker N
│   ├── llm_names.rs        #   Opt-in LLM naming (name_speakers): bounded excerpt, quoted evidence
│   ├── summary.rs          #   Opt-in summary + title through [llm]: rules, map-reduce over parts, JSON reply
│   ├── split.rs            #   Cut long tracks into backend-sized chunks at silence
│   └── render.rs           #   Markdown transcript rendering (front matter, ## Summary, turns)
├── audio/
│   ├── capture.rs          # cpal audio capture
│   ├── wav.rs              # PCM-to-WAV encoding
│   ├── playback.rs         # TTS audio playback
│   ├── feedback.rs         # Audio cues
│   ├── quality.rs          # Quality report: analyze (pure, open profile), verdicts, skip labels with levels
│   └── recovery.rs         # Save/load audio on transcription failure
├── transcription/
│   ├── mod.rs              # TranscriptionBackend trait, timed segments, phrase fallback
│   ├── factory.rs          # Build a backend by name; model, alias and recording backend rules
│   ├── verbose_json.rs     # Whisper verbose_json parsing (segment timestamps)
│   ├── retry.rs            # 429/503 backoff for long jobs (one retry layer per request)
│   ├── flac.rs             # Recording chunks as FLAC for Deepgram, Groq, OpenAI whisper-1 (WAV fallback)
│   ├── deepgram.rs         # Deepgram Nova API (REST + WebSocket streaming)
│   ├── groq.rs             # Groq Whisper API (chunked HTTP, timestamp dedup)
│   ├── openai_realtime.rs  # OpenAI Realtime API (WebSocket, true streaming)
│   ├── openai_realtime_protocol/  # Shared wire/engine/profile for realtime backends
│   ├── openai_compatible_realtime.rs  # Lemonade + other OpenAI-compatible realtime
│   ├── openai_rest.rs      # OpenAI REST API (simple HTTP POST)
│   ├── asr_sidecar.rs      # Generic HTTP ASR sidecar backend
│   ├── local_whisper.rs    # Local whisper.cpp via whisper-rs (feature-gated)
│   ├── phrase_split.rs     # Silence-delimited phrase segmentation (default since #57)
│   ├── local_vosk.rs       # Vosk backend stub (coming soon)
│   └── local_parakeet.rs   # Parakeet/NVIDIA backend stub (coming soon)
├── tts/
│   ├── mod.rs              # TtsBackend trait + create_backend (read selection aloud)
│   ├── groq.rs             # Groq TTS (OpenAI-compatible /v1/audio/speech)
│   ├── openai_compat.rs    # OpenAI + tts-sidecar (local OpenAI-compatible server)
│   └── deepgram_aura.rs    # Deepgram Aura-2 TTS (voice encoded in model id)
├── window/
│   ├── mod.rs              # WindowTracker trait + auto-detection
│   ├── hyprland.rs         # Hyprland window tracking
│   ├── niri.rs             # Niri window tracking
│   ├── sway.rs             # Sway window tracking (swayipc)
│   ├── x11.rs              # X11 window tracking (x11rb)
│   └── dbus.rs             # GNOME/KDE window tracking (zbus D-Bus)
├── hotkey/                 # Hotkey listener (raw evdev) + spec parsing
├── tray/                   # Tray icon (ksni, feature-gated)
├── overlay/                # Recording pill + agent overlay (feature-gated), each on its own thread
│   ├── service.rs          #   Overlay service: picks and drives a backend for each
│   ├── render.rs           #   Pill rendering: theme, animation state, frame drawing
│   ├── wayland.rs          #   Pill Wayland layer-shell backend
│   ├── x11.rs              #   Pill X11 override-redirect backend (placement/shape helpers shared)
│   ├── layer.rs            #   Shared layer-shell bits: create_layer, layer_action, ARGB copy
│   ├── agent_host.rs       #   Agent overlay hosts' shared part: AgentFrames, ShownFlag, FRAME pacing
│   ├── agent_wayland.rs    #   Agent overlay Wayland host: poll + wake socket, no commits while hidden
│   ├── agent_x11.rs        #   Agent overlay X11 host: shaped click-through window, Xft.dpi scale
│   ├── agent_render.rs     #   Agent overlay drawing (headless): AgentScene (stateful: entrance, crossfade), cards
│   ├── agent_orb.rs        #   orb style shapes, one per state
│   ├── agent_face.rs       #   face style shapes, one per state
│   ├── text.rs             #   fontdue text: glyph cache, coverage blend
│   ├── font/               #   Inter Regular + SemiBold ASCII subsets, OFL.txt, the pyftsubset command
│   └── gnome.rs            #   GNOME overlay D-Bus broadcaster (pill state + AgentsChanged JSON)
└── config/
    ├── types.rs            # Config structs, defaults, validation (config.toml)
    ├── setup.rs            # Interactive onboarding (whisrs setup)
    └── edit.rs             # whisrs config interactive editor
```

Injection **policy** lives in the daemon: `type_text_at_cursor`, `paste_via_keyboard`
and `inject_text` (all in `src/daemon/main.rs`) decide what gets sent.
The **primitives** they call live in the `xkb-type` workspace crate, not in `src/`:

```
crates/
├── xkb-type/src/           # 0.2.0 — key injection
│   ├── lib.rs              #   KeyInjector + ClipboardBackend traits
│   ├── keyboard.rs         #   uinput virtual keyboard (evdev)
│   ├── keymap.rs           #   XKB reverse lookup (char → keycode+modifiers)
│   ├── clipboard.rs        #   Clipboard ops (wl-copy/arboard, save/restore)
│   └── wayland_vk.rs       #   zwp_virtual_keyboard_v1 backend
├── asr-dedup/              # 0.1.0 — timestamp + n-gram dedup for chunked ASR
├── audio-silence-gate/     # 0.2.0 - earshot VAD + RMS gate, auto-stop, level meter
├── filler-remove/          # 0.1.0 — filler-word stripping
└── prompt-echo/            # 0.1.0 — prompt-echo hallucination filter
```

Workspace crates use path + version deps. `cargo publish` strips the path and resolves
from crates.io, so **a changed crate must be bumped and published before whisrs is**.

### Supporting Files

```
contrib/
├── 99-whisrs.rules         # udev rule for /dev/uinput access
├── whisrs.service          # systemd user service
├── whisrs.1                # man page for whisrs CLI (.TH version asserted by test)
├── whisrsd.1               # man page for whisrsd daemon (.TH version asserted by test)
├── asr-sidecars/           # Local ASR sidecar recipes
├── agent-skill/            # whisrs-recordings/SKILL.md: lets an agent find and read recordings (read-only)
└── gnome-shell-extension/  # GNOME overlay extension
scripts/
├── dev-install.sh          # Build + install + restart daemon (the dev loop)
├── check-issue55.sh        # Scripted local-whisper repetition + coverage gate
├── gen-issue55-fixture.sh  # Synthesize WAV fixtures (espeak-ng + ffmpeg)
├── check-issue208.sh       # Scripted segmentation "none" loop + coverage gate (large-v3-turbo)
├── gen-issue208-fixture.sh # Synthesize the long-dictation fixture (espeak-ng + ffmpeg)
├── check-vad.sh            # Scripted gate for the speech detector (audio-silence-gate examples/vad_check.rs)
├── gen-vad-fixture.sh      # Synthesize the VAD fixtures (espeak-ng + ffmpeg)
├── check-no-avx512.sh      # Fail if a release binary needs AVX-512 (#150, run by CI)
└── verify-injection.sh     # Injection smoke check
```

`specs/` holds tracked per-feature design docs (e.g.
`specs/openai-compatible-realtime/{DESIGN,README,TASKS}.md`).

**Note on `docs/`:** `/docs/*` is gitignored with a six-file whitelist. Only
`comparison.md`, `configuration.md`, `faq.md`, `gpu-acceleration.md`, `troubleshooting.md` and
`version-roadmap.md` are tracked. Everything else there is local-only, so "fixing the
docs" in an untracked file changes nothing for users and shows up in no PR. Check with
`git check-ignore -v <path>` before claiming a docs fix landed.

## Feature Flags

- `default = ["local-whisper", "tray", "overlay", "hooks", "recording", "mcp"]`
- `local-whisper` — whisper-rs (whisper.cpp) for offline transcription. Requires a C++ toolchain and libclang.
- `tray` — tray icon via `ksni`
- `overlay`: recording pill and agent overlay via `smithay-client-toolkit` + `wayland-client` + `tiny-skia`,
  and `fontdue` for the agent overlay's text (Inter subsets bundled in `src/overlay/font/`, OFL)
- `hooks` — recording-lifecycle hooks + MPRIS media pause via `zbus` (no new system dep; `overlay` already pulls `zbus`)
- `recording`: the recording feature (`src/recording/` submodules, `src/daemon/recording.rs`, the tray's
  recording items and icons, `whisrs recording`, the editor's Recording questions), with `flacenc`, `realfft` and
  `bytes` as its optional deps. The wire and `Config` stay in every build: `Command::Recording`,
  `Response::Recording` and the plain types in `src/recording/mod.rs`, so a CLI and daemon from different builds
  still talk. Without it `whisrs recording ...` still parses and exits 1 ("this whisrs was built without the
  recording feature"), the daemon answers every recording command with "this whisrsd was built without the
  recording feature", `[hotkeys] recording` binds nothing, and dictation's "refused while a recording runs" check is
  compiled out. The transcription layer stays whole (`transcribe_segments` on every backend, verbose_json, retry):
  without the encoder, `flac::recording_upload` passes the WAV through, and nothing sends one
- `mcp`: `whisrs mcp`, a read-only MCP server on stdio (`src/mcp.rs`) via `rmcp` (pinned `~3.5.1`: server, macros,
  transport-io) + `schemars` 1. Implies `recording` (it reads the recordings folder through `recording::store`).
  Without it the `mcp` subcommand still exists and exits 1 with a "built without the mcp feature" error, so it is
  never silently missing
- `vulkan` / `cuda` / `hipblas` — GPU backends for the bundled whisper.cpp, passed
  through to `whisper-rs`. Each implies `local-whisper`. Off by default and never built
  by CI or the release workflow, so they are source-build-only (`cargo install whisrs
  --features vulkan`). Enabling one turns on whisper-rs's `_gpu`, which is what flips
  `WhisperContextParameters::default().use_gpu` to true — there is no whisrs-side wiring.

The **minimal** release build is `--no-default-features --features tray,overlay,hooks,recording,mcp`:
it drops only whisper.cpp. Keep `tray`, `overlay`, `hooks`, `recording` and `mcp` in it (that omission was
issue #51); `mcp` implies `recording`, but both are listed so neither goes missing in a later edit. The list
is in `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `CONTRIBUTING.md`, two comments in
`src/config/types.rs` and here; `flake.nix` builds the default features. A feature left out of that list is
silently ignored at runtime rather than diagnosed: `Config` is not feature-gated, so the key still parses as
known and the unknown-key warning stays quiet. `recording` is the exception: built without it, a `[recording]`
section that differs from the defaults or a `[hotkeys] recording` binding gets one config warning
("this whisrs was built without the recording feature, so [recording] is ignored",
`Config::validate_recording`), since a whole feature going quiet would otherwise look like a bug.

## Coding Conventions

- Use `thiserror` for library-level error types (`WhisrsError` in `src/lib.rs`)
- Use `anyhow` for application-level errors (in binary crates and setup flow)
- Use `tracing` for all logging (not `println!` or `log`). CLI may use `println!` for user output.
- Serde for all serialization: JSON for IPC, TOML for config
- Length-prefixed JSON over Unix socket for IPC (4-byte big-endian length + JSON body)
- All platform-specific behavior behind traits (`KeyInjector`, `WindowTracker`, `ClipboardBackend`)
- **A trait method with a useful default is a trap. Prefer a required method, or check every implementor.** Case study: `WindowTracker::get_focused_window_class()` defaulted to `None` and only Hyprland and Niri overrode it, so `is_terminal` was silently false on KDE, GNOME, Sway and X11 until #71. The method is required now, Sway and X11 implement it, and KDE/GNOME return an explicit `None` (GNOME needs a shell extension, issue #72; KDE would need `org_kde_plasma_window_management`, issue #127), so a new tracker cannot inherit the gap without a compile error.
- **Wire new features into both transcription paths.** A feature added only to `run_streaming_pipeline` silently no-ops for batch backends (`groq`, `openai`/`deepgram` REST, `asr-sidecar`). This shipped as a bug in #54.
- Config structs derive both `Serialize` and `Deserialize` for read/write

## IPC Protocol

Socket: `$XDG_RUNTIME_DIR/whisrs.sock` (fallback: `/tmp/whisrs-<uid>.sock`)

Commands (`Command` in `src/lib.rs`, tagged by `cmd`):

```json
{"cmd": "toggle"}                    // optional "language" overrides general.language for one session
{"cmd": "cancel"}
{"cmd": "status"}
{"cmd": "log", "limit": 20}
{"cmd": "clear-history"}
{"cmd": "command"}                   // command mode: selection → voice instruction → LLM rewrite
{"cmd": "speak"}                     // alias: "read". Repeat or cancel stops playback.
{"cmd": "speak-text", "text": "..."} // speak the text; its own variant so an older daemon errors
                                     // instead of reading the selection
{"cmd": "agent", "action": "list|toggle|send|reset|stop", "name": "jarvis", "text": "...", "new": true}
                                     // all but action optional (name required but for list);
                                     // "new" on toggle (--new) or send starts a fresh session,
                                     // reset (--reset) only forgets it;
                                     // list/send/reset/stop answer with an Agents response
{"cmd": "agent-event", "payload": {...}}  // a harness hook's stdin JSON, from `whisrs agent-hook`
{"cmd": "recording", "action": "start|stop|cancel|status|toggle|retry|copy|name|summarize", "title": "...", "id": "...",
 "language": "de", "mic_language": "de", "system_language": "auto", "others": "Video",
 "shared_mic": true, "num_speakers": 3, "participants": ["Sarah", "Omar"],
 "speaker": "S2", "name": "Sarah"}
                                     // all but action optional: title (start/toggle), id (retry/copy/name/summarize,
                                     // default "last"), speaker + name (name; "" clears), the languages, shared_mic, num_speakers and
                                     // participants (--with) (start/toggle/retry;
                                     // a track's own language wins over "language"), others = system track
                                     // label (start/toggle). Validated by the daemon
                                     // (RecordingOptions::validated)
```

Responses: `{"status": "ok", "state": "idle"}`, `{"status": "error", "message": "..."}`,
a `History` variant carrying log entries, a `Recording` variant carrying the recording status, and
an `Agents` variant carrying agent rows (`{"status": "agents", "agents": [...]}`).

Adding a command means a new `Command` variant plus a daemon handler. Never put logic in
`src/cli/main.rs`: it is a thin socket client. One read-only exception: `whisrs recording
list/show/path` read the recordings folder directly, which keeps transcript bodies off IPC
(as does the speaker list `whisrs recording name` prints after the daemon renamed one).
`whisrs mcp` reads it too, but its logic is in the library (`src/mcp.rs`); the CLI only
starts it with `store::configured_recordings_dir()`.
`whisrs agent-hook` (hidden, run by the hooks whisrs puts in the Claude sessions it starts)
only forwards its stdin to the daemon as `agent-event`, and always exits 0 within two seconds.

## Configuration

Path: `~/.config/whisrs/config.toml` (permissions: 0600)

Transcription backends: `deepgram`, `deepgram-streaming`, `groq`, `openai-realtime`, `openai-compatible-realtime`, `openai`, `local-whisper`, `local-vosk`, `local-parakeet`, `asr-sidecar`

TTS (read selection aloud): the `[tts]` section (`enabled` off by default) drives `whisrs speak` / `read` and `[hotkeys] speak`. Backends: `groq`, `openai`, `deepgram`, `tts-sidecar` (local OpenAI-compatible server, alias `openai-compat`). The TTS key falls back to the matching transcription key (`[groq]`/`[openai]`/`[deepgram]`) unless `[tts] api_key` is set; `tts-sidecar` needs none.

Agent mode: `[[agents]]` entries (`name`, `hotkey`, `new_session_hotkey`, `harness` = `claude|opencode|codex|copilot`, `dir`, `permissions` =
`auto|bypass|ask`, `command`, `args`, `reply_instruction`, `feedback` = `summary|brief|conversational`;
`agent::resolve_agents` validates and skips bad entries, shared by `Config::validate` and the runtime),
plus `[agent_overlay]` (`enabled` follows `[general] overlay`, `style` = `orb|face`, `corner`). With none, nothing runs
except one `systemctl --user list-units` at startup (the scope sweep), and no file is written.
A voice turn records like command mode's instruction (`BatchOrigin::Agent`), then the
turn runs on its own task; replies queue in a FIFO spoken when the state is Idle and no recording
captures (`may_speak`), errors too (`spoken::spoken_error`). A `ReplyHold` (a command or
`[[llm_commands]]` press until it records, a recording start) also holds them, checked under the
dictation lock in `speak_text`. A dictation toggle or command press stops an agent reply
(`speak::stop_agent_playback`, by `DaemonState::tts_origin`), never a selection read-aloud; a
recording start stops it too and requeues it at the front (`agent::hold_for_recording`). An agent
press refused while its turn runs stops only that agent's own reply or notice and then queues no
notice (`AgentRuntime::is_playing_for`); another agent's reply plays on, the notice after it. The Claude driver starts `claude --bg` sessions with `--settings` hooks (`Stop`,
`Notification`) that run `whisrs agent-hook --socket <sock>`; under a systemd service the `--bg`
launch runs in its own `systemd-run --user --scope` (Claude Code's background service must not
live in the whisrs unit), with `--expand-environment=no` (systemd 254+ expands `$VAR` in a scope's
command line; an older one refuses the option and is retried without it, `claude::expand_option_refused`). The next turn is refused while `claude agents` lists the session
`working`, else `claude stop`, a wait until its listed pid has exited (`Exits::wait_for_exit`:
`claude stop` returns, and the listing drops the pid, before Claude Code settles it; capped at 10 s,
then the turn fails), then `claude --bg --resume <id> -- <text>` with no other flag (any flag, or a
resume before the settle, starts a copy without the hooks or `--remote-control`). Every listed pid
is remembered with its `/proc` start time until seen to exit, so a session already listed with no
pid is waited for too (`Exits::settle`). A copy is never adopted: stopped, waited for and
`claude rm`-ed on the launch task (never the session's own bg id, `claude::copy_id`), the resume
tried once more, a second copy fails the turn (`claude::cannot_continue`); only a `Stop` received after that launch ends it (an earlier one is the old
process's own reply, `TurnUpdate::SessionReply`). A `Stop` with no turn running is the session's own
reply and is spoken, unless it repeats a turn that ended on a poll (`TurnOutcome::PolledReply`); a
permission prompt with no turn running marks needs-you. A session working on its own refuses a turn
(`TurnOutcome::Busy`, "It's still running a background task."). The child gets the daemon's env minus
`claude::PARENT_SESSION_VARS` and `WHISRS_*`. Only stored ids are ever passed to `claude`.
A process-per-turn harness (OpenCode, Codex, Copilot) implements `process::TurnHarness` only (`label`, `launch`
-> `Launch` {args, stdin, env, session: an id it names up front}, `strips_env`, `events`: one stdout
line -> `StreamEvent`s Session / Text {message} / Tool / Error / RecoverableError (fails the turn only
when no text follows), `stderr_failure`: a failure it
recognizes in stderr, else `None`); `process::ProcessDriver` does the rest: spawn from a thread that
waits for the child (own process group, own transient scope `whisrs-agent-<instance>-<name>-<turn>.scope`
via `systemd-run --user --scope` when it works, else direct; `<instance>` hashes the `agents.json`
path, so two daemons never touch each other's scopes; `PWD` = dir, stdin the prompt then closed,
`PR_SET_PDEATHSIG` = SIGINT so a turn dies with the daemon), registers it and, in one step with the
runtime's stop, reports the launch's new session id (`SessionRef::pending` until a Text/Tool shows the
agent working in it) or sees the stop (then stops the process itself, `stop_detached`); `StreamState`
(`InterimTracker` on Text/Tool, the reply = the last message's text parts; output lines capped,
`STDOUT_LINE_MAX`, `STDERR_LINE_MAX`), the outcome (an Error event fails, else the reply, else stderr),
`--stop` (SIGINT, SIGTERM, SIGKILL to the group, every process of the tree seen (`Tree`, so a `setsid`
child is reached) and the scope (`systemctl --user kill`), `STOP_GRACES`; a dropped turn is stopped
too, `Registered`), `TurnScopes::sweep` (at startup, also with no agents, `agent::sweep_without_agents`,
and awaited by every turn and rewatch: stops anything a previous daemon's turns left in their scopes, a
background job included), `rewatch` = `process::cut_off` (the turn ended with the
old daemon). No hooks, always Idle between turns. Agent dirs are absolute (`agent::absolute_dir`).
OpenCode: whisrs mints the `ses_` id (`opencode::new_session_id`), a turn of a new or pending session
passes `--title whisrs-<name>` + `args` and prefixes the voice instruction to the prompt, `bypass` =
`--auto`, `ask` = shell/edit ask rules appended to `OPENCODE_CONFIG_CONTENT` (`opencode::ask_config`;
OpenCode's saved "always" approvals still win over them).
Codex (tested against a mock model only): the id is the `thread.started` UUID (only a UUID is ever
resumed, `codex::is_thread_id`); every turn passes the permission flags (`auto` = `--approve-for-me`,
`bypass` = `--dangerously-bypass-approvals-and-sandbox`, `ask` = `--sandbox read-only`),
`-c analytics.enabled=false` (else the exit, and the reply, waits seconds for a metrics upload),
`-c developer_instructions=<TOML string>` (survives compaction only when passed every turn) and `args`,
then `--skip-git-repo-check` (it ends a many-valued `-i` of `args`), all before `resume <id>`;
`item.started` of a tool -> Tool, `item.completed` agent_message -> Text (an empty one too: no reply),
`turn.failed` -> Error (a 401 names `CODEX_API_KEY` when it is set); `RUST_LOG` and
`codex::PARENT_SESSION_VARS` are stripped.
Copilot (tested against a mock model only, offline BYOK; with a GitHub account every turn uses the
user's Copilot plan usage: AI credits, or premium requests on the legacy billing platform): whisrs mints a UUID (`copilot::new_session_id`); a new or pending session
passes `--session-id=<id>` (creates or continues), a confirmed one `--resume=<id>` (fails when Copilot
lost it, `stderr_failure`); the prompt goes on stdin (no `-p`, nothing in `ps`); every turn passes the
permission flags (`auto` = `--allow-all-tools`, `bypass` = `--yolo`, `ask` = `--deny-tool=shell
--deny-tool=write`) and `args`; the voice instruction is a file under `<data>/whisrs/copilot/<name>/`
(`.github/instructions/whisrs-voice.instructions.md`) whose dir is appended to
`COPILOT_CUSTOM_INSTRUCTIONS_DIRS` every turn (lands in the system prompt, never in history);
`assistant.message` -> Text (+ Tool of its first `toolRequests` entry: the interim at once; a sub-agent's
skipped), `tool.execution_start` -> Tool, `session.error` -> Error (`hook` errors ignored, `model_call` ones
RecoverableError), a `policy_blocked`/`compaction_static_context_blocked` warning and `abort` -> Error
(an interrupt exits 0), every sub-agent event skipped (`agentId`, then `data.agentId`, then the
deprecated `parentToolCallId`), `result` -> Session; `copilot::PARENT_SESSION_VARS` (with
`COPILOT_ALLOW_ALL` and `COPILOT_PROMPT_TURN_DELIMITER`) are stripped.
`DaemonContext::state_tx` is a `StateBus` (`context.rs`), sent only by `publish(&ds)`: `raw` (the state
machine: hooks, the reply queue), `ui` (Idle while the voice is an agent's, `DaemonState::agent_voice`:
tray, pill, GNOME pill) and `voice` (which agent, for the board). The runtime's board channel
(`BoardEvent`) and `voice` feed `board_loop`, which owns the `Board` and publishes rows on
`DaemonContext::agent_board` for the agent overlay; a session working or waiting on its own shows
there until a `Stop` or `idle_loop`'s listing ends it (one `session_phases` per harness, 10 s
backing off to 120 s, reset by a turn start or a row change). `TranscriptTail` (`claude_tail.rs`) sends
`Activity` and `Interim` updates; `ProgressGate` admits interims per `feedback`.

Speech detection: `[audio] vad` = `earshot` (default) | `rms` (the 0.1.31 loudness test), read through
`AudioConfig::vad()` (an unknown value warns and uses earshot). Every speech decision on both
transcription paths goes through `audio-silence-gate`: `close` for dictation, command mode,
`[[llm_commands]]` and agent turns (auto-stop, the batch gate, local-whisper's streaming phrase splits),
`open` for recording tracks and cut points (`capped_phrases`, the segment fallback); on a recording's
system track `open` goes without the soft threshold (`TrackKind::vad_config`, `TranscriptionConfig::soft_voices`:
its mask, chunk cuts, meters and a backend's phrases), since it passes music. The `close` relative
floor never rises above the RMS threshold (-50.5 dBFS), so a loud burst cannot bury the speech after it
(`check-vad.sh`'s "Oh!" rows). A frame 6 dB over the background (pre-emphasized, the quietest frame of
the last 240 ms of sound) two frames running needs only score 0.25 (`soft_threshold`), so a soft voice
counts; such frames count half toward arming and the gate, and only within their run of speech (304 ms
with no voiced frame forgets them, `vad::Evidence`), so separate noise onsets never add up
(`check-vad.sh`'s noise-burst row; its real-voice rows need the local `fixtures/vad/real-soft.wav`). A skip toast names its reason with a level (`audio::quality::skip_label`:
"silent (peak -82 dB)", "too quiet (speech at -68 dB)", "without speech (background at -48 dB)"), and
adds "Is the mic muted?" only at a peak of digital silence (`skip_hint`); a streaming dictation whose
audio held no speech says "Nothing typed: the audio was ..." (`notify_state`). Each turn logs one info
line of levels, ending in what was heard (`NoSpeechTimeout::heard_label`: "heard at 0.35 to 8.20 s",
"heard nothing", ", text at 9.50 s"; batch dictation logs it as "dictation capture ended: ..."). No-speech
timeout (`pipeline::NoSpeechTimeout`, fed a `Heard` by all four capture loops): dictation (streaming in
`run_streaming_pipeline`, batch on its `collect_batch_dictation` task, `DaemonState::batch_audio`),
command mode, `[[llm_commands]]` and agent turns (`collect_instruction_audio`) stop and discard once
`no_speech_timeout` (10 s, or `silence_timeout_ms` when longer) passes with nothing heard, counted from
the start or from the end of the latest heard frame (`AutoStopDetector::last_heard`), while auto-stop
is not armed, with the origin's skip-toast wording and rule ("Stopped: nothing heard for 10 s",
`report_nothing_heard`). Heard is the earliest sign of a voice, never auto-stop's arming:
`AutoStopDetector::heard_voice` (any frame voiced before the level floors and the arming count, so a
voice too quiet to arm counts; under `rms` any 16 ms over -50.5 dBFS). A key click, a knock or a noise
switching on counts too (40 of 102 clips of `check-vad.sh`'s noise-burst row are heard), but only
starts the wait again: the row asserts every clip stops within 10 s of its last burst (17.2 s at the
latest). On the streaming path the first transcript text reaching `run_typing_batcher` (`text_heard`)
turns the stop off for good: once text is typed, only auto-stop or the press ends the session. Each
capture logs the first thing heard once at info ("heard at 4.15 s (detector, soft rule, score 0.31,
-48.6 dBFS)" or "(transcript)"). A voice too quiet to arm keeps starting the wait again while it lasts;
when the stop then fires, `AutoStopDetector::too_quiet` names it ("Stopped: the audio was too quiet
(speech at -68 dB)"; command mode and agents "..., stopped"; agents still say "I didn't catch that."),
and a press before that says "too quiet" only when the floor-rejected voice still reached 160 ms of
evidence before the floors (`unfloored_evidence`), else "without speech" (`synthetic_voice` at -60 to
-66 dBFS is heard, never armed, `NoSpeech`). On the batch paths nothing it discards would have been sent:
the gate uses the same detector, so a buffer that never armed fails it. `silence_timeout_ms = 0` turns it off
too, and recordings never use it.

Word replacements: the top-level `[replacements]` table (`"package build" = "PKGBUILD"`) rewrites every dictation on both paths, and every recording transcript segment, after filler removal (`src/replacements.rs`).

Recording (mic + system audio, transcribed after the stop): the `[recording]` section (`backend`,
`user_name`, `language`, `mic_language`, `system_language`, `mic_device`, `system_device`,
`max_duration_minutes`, `audio_retention_days`, `dir`, `diarization`, `diarization_url`,
`num_speakers`, `shared_mic`, `name_speakers`, `summary`, `summary_instruction`)
plus `[hotkeys] recording`. Recordings live in `~/.local/share/whisrs/recordings/<id>/`
(`recording.json`, `<id>.md`, `audio/`, and a `.cache/` of finished chunks kept until the
transcript is written or the audio is deleted; a diarization sidecar's voices stay until the
audio is deleted). The recording backend
must be file-capable, a backend that can transcribe a recorded file: groq, openai, deepgram,
asr-sidecar or local-whisper (`deepgram-streaming` maps to `deepgram`; realtime backends are
refused). Every processing job goes through one queue in `daemon/recording.rs` (`reserve` +
`dispatch`; a `Job` is `Transcribe` or `Summarize`, and the worker runs waiting transcriptions
before waiting summaries, `next_job`; one worker, so a running summary delays the next
transcription); short rewrites that are no job (`recording name`, the retention sweep)
`hold`/`release` the recording instead, and a hold waits only for a transcription
(`blocks_a_hold`). `process_recording` ends at the transcript (and clears `summary_pending`);
with `[recording] summary` the worker then queues a `Summarize` job (`queue_summary`, which
sets `summary_pending: "auto" | "asked"` so `resume_summaries` re-queues it at startup, an
auto one only while `summary` is on). A summary job reads a snapshot, calls the LLM unheld,
then holds the recording just to merge its fields into `recording.json` as it is then
(`summary::write_result`, stale if a name changed meanwhile) and re-render `<id>.md`
(`process::save`); it never re-transcribes. A retry or a rename over a summary marks it
`summary_stale`.
Each `TrackMeta` has `quality: Option<TrackQuality>` (plain type, every build; `None` in an older
`recording.json`): the track writer's meter on the raw chunks (`TrackStats::quality`, stored by
`apply_outcomes`), else measured from the raw WAV by `process_recording`, plus the mic's `echo_path_db`
when echo removal ran; `warnings` are `audio::quality::Check` codes. The front matter gets an `audio:`
block (and `audio_warnings:` only when warned) for each track with a `quality` (an unmeasured one prints
nulls), so an older recording, which has none, renders byte for byte as before. The mic writer's `LiveQuality` raises clipping (every 30 s once 30 s of speech
was heard), too quiet and noisy (once 30 s of the user's own speech was) and `system-silent` (2 minutes
of speech while `SystemSignal::heard` is false), each once per recording, through
`MicAlarm::on_warning`; the muted-mic alarm (`on_silence`) re-arms as before. The system writer marks
each chunk above digital silence on the shared `SystemSignal`; mic chunks taken within
`PLAYBACK_HANGOVER` (300 ms) of one are left out of the mic's speech level, background and SNR
(`QualityMeter::feed_marked`, live and stored; clipping and DC offset count everything), and once any
were, a stored speech level needs 10 s of own speech (`OWN_SPEECH_FOR_LEVEL`). Too quiet, noisy, clipping, DC offset and strong
echo are mic-only warnings (`TrackQuality::from_report(kind, ..)`); the system track's one is
`system-silent`, stored by `apply_outcomes` on an unmeasured quality (`TrackQuality::unmeasured`) when
its writer measured nothing, and kept when the worker measures the WAV.

Environment variable overrides:
- `WHISRS_DEEPGRAM_API_KEY` — overrides `[deepgram] api_key` (also used by the `deepgram` TTS backend)
- `WHISRS_GROQ_API_KEY` — overrides `[groq] api_key` (also used by the `groq` TTS backend)
- `WHISRS_OPENAI_API_KEY` — overrides `[openai] api_key` (also used by the `openai` TTS backend)
- `WHISRS_ASR_SIDECAR_API_KEY` — overrides `[asr-sidecar] api_key` (optional bearer token for authenticated sidecar endpoints)
- `RUST_LOG` — controls daemon log verbosity

## CI Checks

**IMPORTANT: Never push without running all CI checks locally first.** Failing CI generates error emails and clutters the commit history with fix-up commits. Always run these before pushing:

```fish
cargo fmt                                      # fix formatting
cargo clippy --all-targets -- -D warnings      # lint (must pass clean)
cargo clippy --all-targets --no-default-features --features tray,overlay,hooks -- -D warnings             # without recording (CI runs it)
cargo clippy --all-targets --no-default-features --features tray,overlay,hooks,recording -- -D warnings   # without mcp (CI runs it)
cargo test                                     # all tests must pass
cargo test --no-default-features --features tray,overlay,hooks   # the "built without recording" answers (CI runs it)
cargo test -p xkb-type --all-features -- --skip us_qwerty_fr_typeable_via_uinput --skip from_localectl_returns_layout_when_available   # xkb-type (CI runs it; root `cargo test` skips it)
cargo build                                    # must compile
```

If any check fails, fix the issue before pushing. Do not push with the intent to "fix it in the next commit".

**IMPORTANT: Always commit `Cargo.lock` alongside `Cargo.toml` changes.** The root package ships binaries, so `Cargo.lock` is tracked: it makes builds reproducible and is required for `cargo install --locked`. Every commit that modifies dependencies must include the updated lock file.

## Releasing a New Version

When a feature or set of changes warrants a version bump:

0. **Workspace crates first.** If anything under `crates/` changed since the last tag
   (`git log --oneline <last-tag>..HEAD -- crates/`), bump that crate's own version, and
   the root `Cargo.toml` requirement too if the bump falls outside it. Publish those
   crates to crates.io **before** whisrs: `cargo publish` strips the path dep and
   verifies against crates.io, so an unpublished change fails the whisrs publish.
1. **Bump version** in `Cargo.toml`, `flake.nix`, and the `.TH` lines of `contrib/whisrs.1` and `contrib/whisrsd.1` (semver: `MAJOR.MINOR.PATCH`). `cargo test` fails if the man page versions are stale, but the revision date in the same `.TH` line is only checked by eye — bump it too.
2. **Always include `Cargo.lock`** in the version bump commit
3. **Run all CI checks** locally (see above)
4. **Commit** and **push**
5. **Tag and release on GitHub**: `git tag v<VERSION>; git push origin v<VERSION>`, then create a GitHub release with `gh release create v<VERSION>` including release notes summarizing the changes
6. **Publish to crates.io**: Always run `cargo publish` after pushing a version bump — do not skip this step
7. **Update `whisrs-bin`** in its own repo (not this one): bump `pkgver` in the `PKGBUILD`, refresh both `sha256sums_x86_64` and `sha256sums_aarch64` against the new release tarballs, regenerate `.SRCINFO` with `makepkg --printsrcinfo > .SRCINFO`, commit, and push to AUR. Do it **after** step 5, since the assets do not exist until the release workflow finishes.
   **Never bump `whisrs-git` for a release.** It builds `main` HEAD and `pkgver()` derives the version at build time, and the AUR rules forbid pkgver-only commits on VCS packages. The per-release bumps also made it look tag-pinned on the AUR page. Push to it only when the build itself changes (deps, flags, installed files)

## Packaging

Packaging files (AUR PKGBUILD, etc.) do NOT belong in this repo. They are maintained externally:
- **AUR**: two packages we maintain locally and push via `makepkg --printsrcinfo > .SRCINFO; git push`
  - `whisrs-bin` (`~/Projects/whisrs-bin/`) — prebuilt release tarballs, pinned to a tag. The recommended install
  - `whisrs-git` (`~/Projects/whisrs-git/`) — builds from `main` HEAD, no tag pin. Not bumped on release (see step 7)
  - Not ours: graysky maintains the `whisrs` split package (`whisrs`, `whisrs-cuda`, `whisrs-vulkan`), which compiles the tagged release source. Its CUDA and Vulkan flavors are the only packaged GPU builds
- **Nix**: `flake.nix` lives in-repo (standard practice for Nix projects)
- **crates.io**: `cargo publish` manually after version bump

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
