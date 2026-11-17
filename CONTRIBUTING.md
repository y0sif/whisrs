# Contributing to whisrs

Thanks for considering a contribution to whisrs.

## Ways to Contribute

### Desktop and distro reports

When something works differently on your desktop or distro than
[Supported Environments](README.md#supported-environments) says, open an issue with the
Compositor / Distro Test Report template. Run the daemon as `RUST_LOG=debug whisrsd` for the logs.

### Bug reports

Open an issue with the bug report template: it asks for the steps, your environment and the
daemon logs.

### Code contributions

1. Fork the repo
2. Create a feature branch (`git checkout -b feature/my-thing`)
3. Make your changes
4. Run the [checks](#checks)
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
feature and `whisrs mcp` (which needs it).

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

The directory map is in [AGENTS.md](AGENTS.md#layout). The five crates under `crates/` are
published to crates.io on their own: their version rule is in [AGENTS.md](AGENTS.md#workspace-crates).

## Checks

CI runs every command below on each pull request. Run them before you push:

```bash
scripts/check-docs.sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test

# Builds without `recording` and without `mcp`
cargo clippy --all-targets --no-default-features --features tray,overlay,hooks -- -D warnings
cargo clippy --all-targets --no-default-features --features tray,overlay,hooks,recording -- -D warnings
cargo test --no-default-features --features tray,overlay,hooks

# The workspace crates (root `cargo test` skips their tests)
cargo clippy -p xkb-type --all-features --all-targets -- -D warnings
cargo test -p xkb-type --all-features -- --skip from_localectl_returns_layout_when_available
cargo clippy -p audio-silence-gate --all-targets -- -D warnings
cargo test -p audio-silence-gate
cargo clippy -p filler-remove -p asr-dedup -p prompt-echo --all-features --all-targets -- -D warnings
cargo test -p filler-remove -p asr-dedup -p prompt-echo --all-features

# The GPU features still resolve (nothing is compiled)
for feature in vulkan cuda hipblas; do
  cargo metadata --locked --format-version 1 --features "$feature" > /dev/null
done

# Release builds, full and minimal, with no AVX-512 in the binaries
GGML_NATIVE=OFF cargo build --release
scripts/check-no-avx512.sh
GGML_NATIVE=OFF cargo build --release --no-default-features --features tray,overlay,hooks,recording,mcp
scripts/check-no-avx512.sh
```

`cargo fmt` fixes what the first check reports.

## Tests

A change adds a test when it fixes a real bug (the test fails without the fix) or adds a
contract: an IPC message's wire format, a documented invariant, a public API. Any other change
extends the test that already covers the behavior, or adds one when none does. A new config
key gets no test of its own: the registry tests check every key's schema row and its line in
`docs/config-reference.md` (`WHISRS_UPDATE_DOCS=1 cargo test` regenerates that page).

- One test per behavior, named for the behavior. The cases of one behavior are rows of one
  table-driven test.
- Build fixtures with the module's existing test helpers before writing new setup.
- Test what whisrs decides, not derives, std or other crates. A serde or clap attribute gets a
  test only when it is a documented contract (an alias, a hidden flag another program calls).
- Keep issue numbers out of test names and comments: the commit message carries them.
- New repro scripts, benchmarks and manual harnesses stay local and are never committed.

The `whisrs config` TUI's render tests compare each screen with a snapshot of its
rows in `src/cli/tui/snapshots/`. After a change to a screen that is meant, rewrite them
with `WHISRS_UPDATE_SNAPSHOTS=1 cargo test --bin whisrs` and read the diff.

## Code Style

Follow the coding conventions in [AGENTS.md](AGENTS.md). Keep dependencies minimal: don't add a
crate for something you can write in 20 lines.

## Commit Messages

Keep them concise. First line is a summary (imperative mood), then a blank line, then details if needed.

```
Add Sway window tracking via IPC

Use swayipc crate to query focused window and restore focus.
Tested on Sway 1.9 with Wayland.
```
