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
whisrs toggle    # start/stop recording
whisrs cancel    # cancel and discard audio
whisrs speak     # read the selected text aloud via TTS (alias: read; press again to stop)
whisrs status    # query daemon state
whisrs restart   # restart the daemon (wraps systemctl --user when present)
whisrs command   # record a voice instruction, rewrite the selection via LLM
whisrs log       # dictation history
whisrs config    # interactive config editor

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
├── service_ctl.rs          # Daemon restart via systemd (shared by CLI and config editor)
├── state.rs                # State machine (Idle → Recording → Transcribing → Idle;
│                           #   read-aloud: Idle → Synthesizing → Speaking → Idle)
├── history.rs              # Dictation history (whisrs log)
├── llm.rs                  # LLM calls for command mode
├── cli/main.rs             # whisrs CLI (thin client, sends commands over socket)
├── daemon/main.rs          # whisrsd daemon (audio, transcription, typing, IPC server)
├── audio/
│   ├── capture.rs          # cpal audio capture
│   ├── wav.rs              # PCM-to-WAV encoding
│   ├── playback.rs         # TTS audio playback
│   ├── feedback.rs         # Audio cues
│   └── recovery.rs         # Save/load audio on transcription failure
├── transcription/
│   ├── mod.rs              # TranscriptionBackend trait
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
├── overlay/                # Recording overlay (feature-gated)
│   ├── service.rs          #   Overlay service: picks and drives a backend
│   ├── render.rs           #   Shared rendering: theme, animation state, frame drawing
│   ├── wayland.rs          #   Wayland layer-shell backend
│   ├── x11.rs              #   X11 override-redirect backend
│   └── gnome.rs            #   GNOME overlay D-Bus broadcaster
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
├── xkb-type/src/           # 0.1.4 — key injection
│   ├── lib.rs              #   KeyInjector + ClipboardBackend traits
│   ├── keyboard.rs         #   uinput virtual keyboard (evdev)
│   ├── keymap.rs           #   XKB reverse lookup (char → keycode+modifiers)
│   ├── clipboard.rs        #   Clipboard ops (wl-copy/arboard, save/restore)
│   └── wayland_vk.rs       #   zwp_virtual_keyboard_v1 backend
├── asr-dedup/              # 0.1.0 — timestamp + n-gram dedup for chunked ASR
├── audio-silence-gate/     # 0.1.0 — RMS energy gate / VAD
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
└── gnome-shell-extension/  # GNOME overlay extension
scripts/
├── dev-install.sh          # Build + install + restart daemon (the dev loop)
├── check-issue55.sh        # Scripted local-whisper repetition + coverage gate
├── gen-issue55-fixture.sh  # Synthesize WAV fixtures (espeak-ng + ffmpeg)
└── verify-injection.sh     # Injection smoke check
```

`specs/` holds tracked per-feature design docs (e.g.
`specs/openai-compatible-realtime/{DESIGN,README,TASKS}.md`).

**Note on `docs/`:** `/docs/*` is gitignored with a five-file whitelist. Only
`comparison.md`, `configuration.md`, `faq.md`, `troubleshooting.md` and
`version-roadmap.md` are tracked. Everything else there is local-only, so "fixing the
docs" in an untracked file changes nothing for users and shows up in no PR. Check with
`git check-ignore -v <path>` before claiming a docs fix landed.

## Feature Flags

- `default = ["local-whisper", "tray", "overlay"]`
- `local-whisper` — whisper-rs (whisper.cpp) for offline transcription. Requires a C++ toolchain and libclang.
- `tray` — tray icon via `ksni`
- `overlay` — recording overlay via `smithay-client-toolkit` + `wayland-client` + `tiny-skia`

The **minimal** release build is `--no-default-features --features tray,overlay`: it
drops only whisper.cpp. Keep `tray` and `overlay` in it (that omission was issue #51).

## Coding Conventions

- Use `thiserror` for library-level error types (`WhisrsError` in `src/lib.rs`)
- Use `anyhow` for application-level errors (in binary crates and setup flow)
- Use `tracing` for all logging (not `println!` or `log`). CLI may use `println!` for user output.
- Serde for all serialization: JSON for IPC, TOML for config
- Length-prefixed JSON over Unix socket for IPC (4-byte big-endian length + JSON body)
- All platform-specific behavior behind traits (`KeyInjector`, `WindowTracker`, `ClipboardBackend`)
- **A trait method with a useful default is a trap.** `WindowTracker::get_focused_window_class()` defaults to `None` and is overridden only by Hyprland and Niri, so `is_terminal` is silently false on KDE, GNOME, Sway and X11. Prefer a required method, or check every implementor.
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
```

Responses: `{"status": "ok", "state": "idle"}`, `{"status": "error", "message": "..."}`,
and a `History` variant carrying log entries.

Adding a command means a new `Command` variant plus a daemon handler. Never put logic in
`src/cli/main.rs` — it is a thin socket client.

## Configuration

Path: `~/.config/whisrs/config.toml` (permissions: 0600)

Transcription backends: `deepgram`, `deepgram-streaming`, `groq`, `openai-realtime`, `openai-compatible-realtime`, `openai`, `local-whisper`, `local-vosk`, `local-parakeet`, `asr-sidecar`

TTS (read selection aloud): the `[tts]` section (`enabled` off by default) drives `whisrs speak` / `read` and `[hotkeys] speak`. Backends: `groq`, `openai`, `deepgram`, `tts-sidecar` (local OpenAI-compatible server, alias `openai-compat`). The TTS key falls back to the matching transcription key (`[groq]`/`[openai]`/`[deepgram]`) unless `[tts] api_key` is set; `tts-sidecar` needs none.

Environment variable overrides:
- `WHISRS_DEEPGRAM_API_KEY` — overrides `[deepgram] api_key` (also used by the `deepgram` TTS backend)
- `WHISRS_GROQ_API_KEY` — overrides `[groq] api_key` (also used by the `groq` TTS backend)
- `WHISRS_OPENAI_API_KEY` — overrides `[openai] api_key` (also used by the `openai` TTS backend)
- `RUST_LOG` — controls daemon log verbosity

## CI Checks

**IMPORTANT: Never push without running all CI checks locally first.** Failing CI generates error emails and clutters the commit history with fix-up commits. Always run these before pushing:

```fish
cargo fmt                                      # fix formatting
cargo clippy --all-targets -- -D warnings      # lint (must pass clean)
cargo test                                     # all tests must pass
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
7. **Update the AUR package** in its own repo (not this one): bump `pkgver` in the `PKGBUILD`, regenerate `.SRCINFO` with `makepkg --printsrcinfo > .SRCINFO`, commit, and push to AUR

## Packaging

Packaging files (AUR PKGBUILD, etc.) do NOT belong in this repo. They are maintained externally:
- **AUR**: `whisrs-git` package on AUR (maintained locally, pushed via `makepkg --printsrcinfo > .SRCINFO; git push`)
- **Nix**: `flake.nix` lives in-repo (standard practice for Nix projects)
- **crates.io**: `cargo publish` manually after version bump

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
