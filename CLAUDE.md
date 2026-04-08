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
whisrs status    # query daemon state

# Debug logging
set -x RUST_LOG debug; whisrsd
```

## Project Structure

Single Cargo package with two binaries:

```
src/
├── lib.rs                  # Shared types: Config, IPC protocol, errors, helpers
├── cli/
│   └── main.rs             # whisrs CLI (thin client, sends commands over socket)
├── daemon/
│   └── main.rs             # whisrsd daemon (audio, transcription, typing, IPC server)
├── audio/
│   ├── mod.rs              # Audio module exports
│   ├── capture.rs          # cpal audio capture + WAV encoding
│   ├── silence.rs          # VAD/silence detection (RMS energy, auto-stop)
│   └── recovery.rs         # Save/load audio on transcription failure
├── transcription/
│   ├── mod.rs              # TranscriptionBackend trait
│   ├── deepgram.rs         # Deepgram Nova API (REST + WebSocket streaming)
│   ├── groq.rs             # Groq Whisper API (chunked HTTP, timestamp dedup)
│   ├── openai_realtime.rs  # OpenAI Realtime API (WebSocket, true streaming)
│   ├── openai_rest.rs      # OpenAI REST API (simple HTTP POST)
│   ├── local_whisper.rs    # Local whisper.cpp via whisper-rs (feature-gated)
│   ├── local_vosk.rs       # Vosk backend stub (coming soon)
│   ├── local_parakeet.rs   # Parakeet/NVIDIA backend stub (coming soon)
│   └── dedup.rs            # Timestamp + n-gram deduplication for chunked APIs
├── input/
│   ├── mod.rs              # KeyInjector trait
│   ├── uinput.rs           # Virtual keyboard via evdev UinputDevice
│   ├── keymap.rs           # XKB reverse lookup (char → keycode+modifiers)
│   └── clipboard.rs        # Clipboard ops (wl-copy/arboard, save/restore)
├── window/
│   ├── mod.rs              # WindowTracker trait + auto-detection
│   ├── hyprland.rs         # Hyprland window tracking
│   ├── sway.rs             # Sway window tracking (swayipc)
│   ├── x11.rs              # X11 window tracking (x11rb)
│   └── dbus.rs             # GNOME/KDE window tracking (zbus D-Bus)
├── config/
│   ├── mod.rs              # Config module exports
│   └── setup.rs            # Interactive onboarding (whisrs setup)
└── state.rs                # State machine (Idle → Recording → Transcribing → Idle)
```

### Supporting Files

```
contrib/
├── 99-whisrs.rules         # udev rule for /dev/uinput access
├── whisrs.service          # systemd user service
├── whisrs.1                # man page for whisrs CLI
└── whisrsd.1               # man page for whisrsd daemon
docs/
├── plan.md                 # Implementation plan (phases 0-7)
├── architecture.md         # System architecture and data flow
├── branding.md             # Name, colors, ASCII banner
└── tech-stack.md           # Technology choices and rationale
```

## Feature Flags

- `default = ["local-whisper"]` — builds with all backends (cloud + local whisper.cpp)
- `local-whisper` — enables whisper-rs (whisper.cpp) for offline transcription. Requires C++ toolchain and libclang. Included by default.

## Coding Conventions

- Use `thiserror` for library-level error types (`WhisrsError` in `src/lib.rs`)
- Use `anyhow` for application-level errors (in binary crates and setup flow)
- Use `tracing` for all logging (not `println!` or `log`). CLI may use `println!` for user output.
- Serde for all serialization: JSON for IPC, TOML for config
- Length-prefixed JSON over Unix socket for IPC (4-byte big-endian length + JSON body)
- All platform-specific behavior behind traits (`KeyInjector`, `WindowTracker`, `ClipboardHandler`)
- Config structs derive both `Serialize` and `Deserialize` for read/write

## IPC Protocol

Socket: `$XDG_RUNTIME_DIR/whisrs.sock` (fallback: `/tmp/whisrs-<uid>.sock`)

Commands: `{"cmd": "toggle"}`, `{"cmd": "cancel"}`, `{"cmd": "status"}`
Responses: `{"status": "ok", "state": "idle"}`, `{"status": "error", "message": "..."}`

## Configuration

Path: `~/.config/whisrs/config.toml` (permissions: 0600)

Backends: `deepgram`, `deepgram-streaming`, `groq`, `openai-realtime`, `openai`, `local-whisper`, `local-vosk`, `local-parakeet`

Environment variable overrides:
- `WHISRS_DEEPGRAM_API_KEY` — overrides `[deepgram] api_key`
- `WHISRS_GROQ_API_KEY` — overrides `[groq] api_key`
- `WHISRS_OPENAI_API_KEY` — overrides `[openai] api_key`
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

**IMPORTANT: Always commit `Cargo.lock` alongside `Cargo.toml` changes.** This is a binary crate — `Cargo.lock` ensures reproducible builds and is required for `cargo publish` and `cargo install --locked`. Every commit that modifies dependencies must include the updated lock file.

## Releasing a New Version

When a feature or set of changes warrants a version bump:

1. **Bump version** in `Cargo.toml` and `flake.nix` (semver: `MAJOR.MINOR.PATCH`)
2. **Always include `Cargo.lock`** in the version bump commit
3. **Run all CI checks** locally (see above)
4. **Commit** and **push**
5. **Publish to crates.io**: Always run `cargo publish` after pushing a version bump — do not skip this step
6. **Update AUR** package: bump `pkgver` in `/home/y0sif/Projects/whisrs-git/PKGBUILD`, regenerate `.SRCINFO` with `makepkg --printsrcinfo > .SRCINFO`, commit, and `git push` to AUR

## Packaging

Packaging files (AUR PKGBUILD, etc.) do NOT belong in this repo. They are maintained externally:
- **AUR**: `whisrs-git` package on AUR (maintained locally, pushed via `makepkg --printsrcinfo > .SRCINFO; git push`)
- **Nix**: `flake.nix` lives in-repo (standard practice for Nix projects)
- **crates.io**: `cargo publish` manually after version bump
