# whisrs: agent guide

whisrs is voice input for the Linux desktop: dictation, read-aloud, LLM rewrites of the selection,
voice turns for coding agents, and recordings of the mic and system audio, transcribed after the
stop. It is a Cargo workspace. The root package builds two binaries
from one library: `whisrsd`, the daemon that owns audio, transcription, typing and the feature runtimes,
and `whisrs`, a thin CLI client that sends one command over a Unix socket. Five supporting crates
live in `crates/`.

## Build and check

```sh
cargo build                                  # debug build, default features (needs a C++ toolchain, cmake, libclang)
cargo test                                   # root package tests
cargo clippy --all-targets -- -D warnings    # lint; warnings are errors
cargo fmt                                    # format
```

The full CI gate, every build-feature combination and the per-crate tests included, is in
[CONTRIBUTING.md#checks](CONTRIBUTING.md#checks). Rules:

- Never push before every check there passes locally.
- Commit `Cargo.lock` with every `Cargo.toml` change. It is tracked because the package ships
  binaries (`cargo install --locked`).

Run it: `whisrs setup` (TUI wizard, needs a terminal), then `whisrsd` or
`systemctl --user enable --now whisrs.service`. `RUST_LOG=debug whisrsd` logs at debug level.
`scripts/dev-install.sh` rebuilds this checkout, installs it to `~/.cargo/bin` (`--system`:
`/usr/local/bin`) and restarts the daemon. CLI subcommands: `whisrs --help` and `contrib/whisrs.1`.

## Layout

| Path | Holds |
|---|---|
| `src/cli/main.rs` | `whisrs`: parses the command, sends it, prints the answer |
| `src/cli/tui/` | `whisrs config` and `whisrs setup` (ratatui, CLI binary only; see its `AGENTS.md`) |
| `src/daemon/` | `whisrsd`: `main.rs` (IPC server, `handle_command`), `context.rs` (shared state, state broadcast), `startup.rs` |
| `src/daemon/` (flows) | `dictation.rs`, `pipeline.rs` (streaming and batch paths), `command_mode.rs`, `injection.rs`, `selection.rs`, `speak.rs`, `retry.rs` |
| `src/daemon/` (the rest) | `agent.rs`, `recording.rs`, `profile.rs`, `hooks.rs`, `notify.rs`, `factory.rs` |
| `src/lib.rs`, `ipc.rs`, `state.rs`, `service.rs` | `WhisrsError` and re-exports; the wire protocol; the state machine; systemd and OpenRC control |
| `src/config/` | config types and validation, the format-preserving write, profiles, vocabulary, key masking, picker catalogs, `setup/` (the wizard's side effects) |
| `src/transcription/` | `TranscriptionBackend`, every backend, the backend factory, phrase splitting, retries |
| `src/tts/` | `TtsBackend` and the read-aloud backends |
| `src/audio/` | capture, devices, playback, cues, the quality report, recovery audio |
| `src/agent/` | agent mode shared by both binaries: config, a driver per harness, spoken replies (see its `AGENTS.md`) |
| `src/recording/` | recording types (every build) and the feature: store, track writer, echo removal, speakers, names, summaries, rendering |
| `src/window/` | `WindowTracker`, one per compositor |
| `src/hotkey/`, `src/tray/`, `src/overlay/` | evdev hotkeys; the tray icon; the recording pill and agent overlay (Wayland, X11, GNOME D-Bus) |
| `src/{hooks,mpris,history,llm,replacements,mcp}.rs` | recording-lifecycle hooks; MPRIS pause; dictation history; LLM calls; `[replacements]`; `whisrs mcp` |
| `crates/xkb-type/` | key injection primitives: uinput, `zwp_virtual_keyboard_v1`, XTEST, XKB keymap lookup, clipboard |
| `crates/audio-silence-gate/` | the speech detector: earshot VAD and RMS gate, auto-stop, speech gate, level meter |
| `crates/{asr-dedup,filler-remove,prompt-echo}/` | chunk dedup for chunked ASR; filler-word removal; prompt-echo filter |
| `contrib/` | man pages, systemd unit, `openrc/`, udev rule, `asr-sidecars/`, `diarization-sidecars/`, `agent-skill/`, GNOME extension |
| `scripts/` | `dev-install.sh`, `check-no-avx512.sh` (CI), scripted fixture checks and their generators |
| `examples/`, `tests/` | repro harnesses, overlay frame export; the built-without-recording test, harness output fixtures |
| `docs/` | user docs; only the pages whitelisted in `.gitignore` are tracked |

## Feature flags

The features and what each pulls in are `[features]` in `Cargo.toml`. All are on by default but
the GPU ones (`vulkan`, `cuda`, `hipblas`), which CI only checks resolve. Rules:

- `Config` and the IPC wire types are not feature-gated: mixed builds still talk, and a left-out
  feature's keys are ignored silently. `recording` and `mcp` say "built without" instead.
- The minimal release build is `--no-default-features --features tray,overlay,hooks,recording,mcp`
  (everything but whisper.cpp). It appears in `.github/workflows/ci.yml`,
  `.github/workflows/release.yml`, `CONTRIBUTING.md` and two comments in `src/config/types.rs`:
  change them together.
- `flake.nix` builds the default features.

## Coding conventions

- Errors: `thiserror` in the library (`WhisrsError` in `src/lib.rs`), `anyhow` in the binaries.
- Logging: `tracing` everywhere. The CLI may `println!` its output.
- Serialization: serde. JSON for IPC, TOML for config. Config structs derive both `Serialize`
  and `Deserialize`.
- Platform behavior sits behind traits: `KeyInjector` and `ClipboardBackend` (`xkb-type`),
  `WindowTracker`, `TranscriptionBackend`, `TtsBackend`.
- A trait method with a useful default is a trap: make it required, or check every implementor.
- A dictation feature goes into both transcription paths in `src/daemon/pipeline.rs`:
  `run_streaming_pipeline` and `transcribe_batch_audio`. One alone silently skips the other
  backends.
- Injection policy (what gets typed or pasted) lives in `src/daemon/injection.rs`; the
  primitives live in `crates/xkb-type`.
- A new config key needs a row in `src/cli/tui/schema.rs`; a test fails without one.

## IPC

- Socket: `$XDG_RUNTIME_DIR/whisrs.sock`, else `/tmp/whisrs-<uid>.sock`. Framing: a 4-byte
  big-endian length, then the JSON body.
- `Command` (tagged by `cmd`) and `Response` (tagged by `status`) are in `src/ipc.rs`.
  `ipc::request_blocking` is the same framing for callers without a tokio runtime.
- Adding a command means a `Command` variant plus its handler in `handle_command`
  (`src/daemon/main.rs`). `daemon/profile.rs` matches every variant, so the compiler asks
  whether it starts work.
- Old and new builds must keep talking. New behavior gets its own variant, so an older daemon
  errors instead of doing something else. `DaemonInfo` is additive: every field has a default,
  and an unknown state parses to `None`.
- The CLI is a thin client: no logic in `src/cli/main.rs`. Read-only exceptions:
  `whisrs recording list/show/path` (and `delete`'s id lookup) read the recordings folder so
  transcripts stay off IPC; `whisrs profile list`, and a switch with the daemon down, go through
  `config::profile`; `whisrs mcp` runs `src/mcp.rs`; the TUI reads and writes config.toml through
  the library.

## Workspace crates

The root package depends on each crate by path and a caret version requirement. `cargo publish`
drops the path and resolves from crates.io, so a crate that changed is bumped and published before
whisrs is, and a bump past the requirement's range edits it too.

## Docs

- How to use a feature lives in `docs/` or the man pages. How a subsystem works, and its
  invariants, live in the module's `//!` comment. This file holds only the rules above. Link to
  the home of a fact instead of repeating it.
- A change to user-visible behavior updates its doc in the same change.
- `docs/` is gitignored except the whitelisted pages: run `git check-ignore -v <path>` before
  editing one.
- No issue numbers, fix history or personal paths in docs or doc comments (those go in the commit
  message), and no internal files (specs, plans, notes) in the repo. `scripts/check-docs.sh` checks the docs.

## Where to read next

- The `//!` comment at the top of the module you change, when present.
- `src/cli/tui/AGENTS.md` and `src/agent/AGENTS.md` for invariants that span those directories.
- `docs/configuration.md` (configuring each feature), `docs/config-reference.md` (every key),
  `docs/agents.md`, `docs/recording.md`, `docs/troubleshooting.md`.
- `contrib/whisrs.1` and `contrib/whisrsd.1` for the CLI and the daemon.
- `CONTRIBUTING.md` for submitting a change.
