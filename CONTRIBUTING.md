# Contributing to whisrs

Thanks for considering a contribution to whisrs. This document covers the basics.

## Ways to Contribute

### Testing on your setup

The most impactful thing you can do right now is test whisrs on your compositor and distro, then report what works and what doesn't.

**Compositors that need testing:**
- Sway / i3
- KDE Plasma (Wayland)
- GNOME (Wayland and X11)
- Any X11 window manager

**Distros that need testing:**
- Fedora, Ubuntu, Debian, NixOS, Void, Gentoo, etc.

When reporting, include:
- Compositor + version
- Distro + version
- Audio backend (PipeWire / PulseAudio / ALSA)
- What worked, what didn't
- Daemon logs (`RUST_LOG=debug whisrsd`)

### Bug reports

Open an issue using the bug report template. Include:
- Steps to reproduce
- Expected vs actual behavior
- Daemon logs
- Your environment (compositor, distro, audio backend)

### Code contributions

1. Fork the repo
2. Create a feature branch (`git checkout -b feature/my-thing`)
3. Make your changes
4. Run the checks: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
5. Commit with a clear message
6. Open a PR using the template

## Development Setup

### Prerequisites

```bash
# Arch Linux
sudo pacman -S base-devel alsa-lib libxkbcommon pkg-config clang cmake

# Ubuntu/Debian
sudo apt install build-essential libasound2-dev libxkbcommon-dev pkg-config libclang-dev cmake

# Fedora
sudo dnf install gcc-c++ alsa-lib-devel libxkbcommon-devel pkg-config clang-devel cmake
```

`clang` and `cmake` are needed because `local-whisper` (whisper.cpp via whisper-rs) is a
default feature. To skip that toolchain, build without it:

```bash
cargo build --no-default-features --features tray,overlay,hooks,recording,mcp
```

That is the minimal release build. Dropping `recording,mcp` too leaves out the recording
feature and `whisrs mcp` (which needs it). CI lints that build and the one without `mcp`, and
runs the tests of the one without `recording`.

### Build and test

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The `whisrs config` TUI's render tests compare each screen with a snapshot of its
rows in `src/cli/tui/snapshots/`. After a change to a screen that is meant, rewrite them
with `WHISRS_UPDATE_SNAPSHOTS=1 cargo test --bin whisrs` and read the diff.

### Running locally

```bash
# Terminal 1: start the daemon with debug logging
RUST_LOG=debug cargo run --bin whisrsd

# Terminal 2: send commands
cargo run --bin whisrs -- toggle
cargo run --bin whisrs -- status
cargo run --bin whisrs -- cancel
```

### Project structure

```
src/
  cli/main.rs          — thin CLI client
  daemon/main.rs       — daemon (the real application)
  lib.rs               — shared types, config, IPC protocol
  state.rs             — state machine
  audio/               — cpal capture, WAV encoding, playback
  transcription/       — backend trait + Groq, OpenAI, local implementations
  tts/                 — text-to-speech backends (read selection aloud)
  window/              — compositor-specific window tracking
  hotkey/              — evdev hotkey listener + spec parsing
  tray/, overlay/      — tray icon and recording overlay (feature-gated)
  config/              — interactive setup and config editor
```

The `whisrs config` TUI is in `src/cli/tui/`, in the CLI binary so whisrsd never links
ratatui: screens, widgets, and the schema of every config key. It reads and writes
config.toml through `src/config/`.

Injection policy stays in the daemon (`inject_text` and friends in `src/daemon/main.rs`),
but the primitives it calls live in the `xkb-type` workspace crate, alongside four other
extracted crates:

```
crates/
  xkb-type/            — uinput keyboard, XKB keymap, clipboard, wayland virtual keyboard
  asr-dedup/           — timestamp + n-gram dedup for chunked ASR
  audio-silence-gate/  — RMS energy gate / VAD
  filler-remove/       — filler-word stripping
  prompt-echo/         — prompt-echo hallucination filter
```

These are published to crates.io independently. A change to one needs a version bump in
its own `Cargo.toml`. The root `Cargo.toml` dependency line only needs editing when the
new version falls outside its existing requirement: all five are caret requirements, so
any 0.1.x bump is absorbed and only a 0.2.0 would force a root edit.

## Code Style

- Use `thiserror` for library error types, `anyhow` for binary error handling
- Use `tracing` for all logging (not `println!` or `log`)
- Run `cargo fmt` before committing
- No warnings from `cargo clippy --all-targets -- -D warnings`
- Keep dependencies minimal — don't add a crate for something you can write in 20 lines

## Commit Messages

Keep them concise. First line is a summary (imperative mood), then a blank line, then details if needed.

```
Add Sway window tracking via IPC

Use swayipc crate to query focused window and restore focus.
Tested on Sway 1.9 with Wayland.
```
