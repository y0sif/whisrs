```
            _     _
 __      __| |__ (_)___  _ __ ___
 \ \ /\ / /| '_ \| / __|| '__/ __|
  \ V  V / | | | | \__ \| |  \__ \
   \_/\_/  |_| |_|_|___/|_|  |___/

  speak. type. done.
```

# whisrs

[![Crates.io](https://img.shields.io/crates/v/whisrs)](https://crates.io/crates/whisrs)
[![docs.rs](https://img.shields.io/docsrs/whisrs)](https://docs.rs/whisrs)

**Linux-first voice-to-text dictation tool, written in Rust.**

Speech-to-text for Wayland, X11, Hyprland, Sway, Niri, GNOME, and KDE. Press a hotkey, speak, and your words appear at the cursor. Works with any app, any window manager, any desktop environment. Supports cloud transcription (Groq, Deepgram, OpenAI) and fully offline local transcription via whisper.cpp. Fast, private, open source. It can also read the selected text back aloud (text-to-speech via Groq, OpenAI, Deepgram, or a local sidecar).

---

## Why whisrs?

Dictation tools like Wispr Flow and Superwhisper are not available on Linux. [xhisper](https://github.com/imaginalnika/xhisper) proved the concept works, but I kept running into limitations. whisrs takes that idea and rebuilds it in Rust as a single async process with native keyboard layout support, window tracking, and multiple transcription backends.

---

## Installation

### Quick install (Linux x86_64 / aarch64)

```bash
curl -sSL https://y0sif.github.io/whisrs/install.sh | bash
```

The install script downloads the latest prebuilt tarball, installs `whisrs`/`whisrsd` to `/usr/local/bin`, and runs interactive setup.

Pin a specific version with `WHISRS_VERSION=v0.1.10` or use the cloud-only minimal build with `WHISRS_MINIMAL=1`. Re-run the same command later to upgrade.

To **build from source** instead — including custom feature flag combos or unsupported architectures — use `cargo install whisrs --locked` or the `whisrs-git` AUR package.

After install, **press your hotkey** to start recording, **press again** to stop. Text appears at your cursor.

<details>
<summary><b>Other install methods (pre-built binary, AUR, Cargo, Nix, manual)</b></summary>

### Pre-built binary (manual)

The Quick install above already does this — this section is for users who want to install the tarball by hand.

Each tagged release publishes tarballs on [GitHub Releases](https://github.com/y0sif/whisrs/releases/latest) with both `whisrs` and `whisrsd` plus the contrib files (udev rule, systemd unit, man pages).

```bash
# Pick the artifact for your arch + variant:
ARCH=x86_64   # or aarch64
curl -sSL -o whisrs.tar.gz https://github.com/y0sif/whisrs/releases/latest/download/whisrs-linux-${ARCH}.tar.gz

# Or the minimal build (cloud backends only — no whisper.cpp):
# curl -sSL -o whisrs.tar.gz https://github.com/y0sif/whisrs/releases/latest/download/whisrs-linux-${ARCH}-minimal.tar.gz

tar xzf whisrs.tar.gz
sudo install -m755 whisrs whisrsd /usr/local/bin/
sudo install -m644 contrib/99-whisrs.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG input $USER   # log out / back in for the group change
whisrs setup
```

| Variant | Architectures | Includes local whisper.cpp |
|---|---|---|
| `whisrs-linux-{x86_64,aarch64}.tar.gz` | x86_64, aarch64 | yes (full build) |
| `whisrs-linux-{x86_64,aarch64}-minimal.tar.gz` | x86_64, aarch64 | no (cloud backends only) |

### Arch Linux (AUR)

```bash
yay -S whisrs-git
```

After install, run `whisrs setup` to configure your backend, API keys, permissions, and keybindings.

### Cargo

```bash
cargo install whisrs
```

Requires system dependencies: `alsa-lib`, `libxkbcommon`, `clang`, `cmake`.

After install, run `whisrs setup`.

### Nix

```bash
nix profile install github:y0sif/whisrs
```

Or add to your flake inputs:
```nix
inputs.whisrs.url = "github:y0sif/whisrs";
```

### Manual install

#### 1. Dependencies

```bash
# Arch Linux
sudo pacman -S base-devel alsa-lib libxkbcommon clang cmake

# Debian/Ubuntu
sudo apt install build-essential libasound2-dev libxkbcommon-dev libclang-dev cmake

# Fedora
sudo dnf install gcc-c++ alsa-lib-devel libxkbcommon-devel clang-devel cmake
```

#### 2. Build

```bash
git clone https://github.com/y0sif/whisrs
cd whisrs
cargo install --path .
```

#### 3. Setup

```bash
whisrs setup
```

The interactive setup will walk you through backend selection, API keys / model download, microphone test, uinput permissions, systemd service, and keybindings.

#### 4. Bind a hotkey

Example for Hyprland (`~/.config/hypr/hyprland.conf`):
```
bind = $mainMod, W, exec, whisrs toggle
```

Example for Sway (`~/.config/sway/config`):
```
bindsym $mod+w exec whisrs toggle
```

</details>

---

## Transcription Backends

| Backend | Type | Streaming | Cost | Best for |
|---|---|---|---|---|
| **Groq** | Cloud | Batch | Free tier available | Getting started, budget use |
| **Deepgram Streaming** | Cloud (WebSocket) | True streaming | $200 free credit | Streaming with free credits |
| **Deepgram REST** | Cloud | Batch | $200 free credit | Simple, 60+ languages |
| **OpenAI Realtime** | Cloud (WebSocket) | True streaming | Paid | Best UX, text as you speak |
| **OpenAI REST** | Cloud | Batch | Paid | Simple fallback |
| **OpenAI-compatible Realtime** | External WebSocket | Completed-utterance realtime | Free / self-hosted | Lemonade and similar OpenAI-style ASR servers |
| **Local whisper.cpp** | Local (CPU/GPU) | Sliding window | Free | Privacy, offline use |
| **ASR sidecar** | Local sidecar | Batch | Free | Bring-your-own local ASR (Moonshine, Parakeet, VibeVoice-ASR, …) |

Groq is the default. For fully offline use, run `whisrs setup` and select **Local > whisper.cpp** — `base.en` (142 MB, ~388 MB RAM) is recommended; `tiny.en` (75 MB) for low-end hardware, `small.en` (466 MB) for higher accuracy.

For local ASR models without a Rust runtime (Moonshine, NVIDIA Parakeet, Microsoft VibeVoice-ASR), use the generic ASR sidecar backend — it talks to a small local HTTP service that hosts the model. See [`contrib/asr-sidecars/`](contrib/asr-sidecars/) for ready-to-run sidecars.

For external realtime servers that speak the OpenAI Realtime transcription event model over WebSocket, use `backend = "openai-compatible-realtime"`. Lemonade is the first supported profile. Unlike OpenAI cloud, Lemonade-style interim partials are replaceable, so whisrs types completed phrases as they stabilize instead of blindly appending every partial hypothesis.

---

## Configuration

Config file: `~/.config/whisrs/config.toml` — `whisrs setup` writes a working file. A minimal example:

```toml
[general]
backend = "groq"   # groq | deepgram-streaming | deepgram | openai-realtime | openai | openai-compatible-realtime | local-whisper | asr-sidecar
language = "en"    # ISO 639-1 or "auto"
overlay = false    # bottom-screen recording overlay

[groq]
api_key = "gsk_..."
```

Env-var overrides: `WHISRS_GROQ_API_KEY`, `WHISRS_DEEPGRAM_API_KEY`, `WHISRS_OPENAI_API_KEY`.

For the full reference (overlay, `[input]`, `[openai-compatible-realtime]`, `[asr-sidecar]`, `[llm]`, `[hotkeys]`, GNOME extension setup), see [docs/configuration.md](docs/configuration.md).

---

## CLI Commands

```
whisrs setup     # Interactive onboarding
whisrs config    # Interactive editor for ~/.config/whisrs/config.toml
whisrs toggle    # Start/stop recording
whisrs cancel    # Cancel recording, discard audio
whisrs status    # Query daemon state
whisrs restart   # Restart the daemon (uses the systemd user service when present)
whisrs command   # Command mode: select text + speak instruction → LLM rewrite
whisrs speak     # Read the selected text aloud (alias: whisrs read; press again to stop)
whisrs log       # Show recent transcription history
whisrs log -n 5  # Show last 5 entries
whisrs log --clear  # Clear all history
```

---

## Supported Environments

| Component | Support |
|---|---|
| **Hyprland** | Tested by maintainer and community (Arch Linux) |
| **Sway / i3** | Implemented; additional reports welcome |
| **Niri** | Implemented; tested by contributor on Niri 26.04 (CachyOS) |
| **X11 (any WM)** | Tested by community on Ubuntu 24.04 (Xorg) |
| **GNOME Wayland** | Tested by community on Ubuntu 24.04 and Arch (mutter); overlay via the bundled [GNOME Shell extension](contrib/gnome-shell-extension/README.md) |
| **KDE Wayland** | Implemented via D-Bus; reports welcome |
| **Audio** | PipeWire, PulseAudio, ALSA (auto-detected via cpal) |
| **Distros** | Confirmed on Arch Linux and Ubuntu 24.04; any Linux with the system dependencies above |

> **Note:** whisrs is daily-driven on Hyprland (Arch Linux), with community confirmation on GNOME Wayland (Ubuntu 24.04 + Arch), Xorg (Ubuntu 24.04), and Niri (CachyOS). Sway, i3, and KDE reports are still wanted — if you use whisrs there, please open an issue with what works and what doesn't.

---

## Project Status

whisrs is functional and usable for daily dictation. Streaming transcription, command mode, read-selection-aloud (TTS via Groq, OpenAI, Deepgram, or a local sidecar), multi-language support, system tray, OSD overlay, layout-aware injection (incl. AltGr + dead keys), the generic ASR sidecar backend (Moonshine, Parakeet, VibeVoice-ASR), and packaging for AUR / Nix / crates.io all ship today. Native local Vosk and Parakeet backends are next.

Per-release details: [docs/version-roadmap.md](docs/version-roadmap.md).

---

## Troubleshooting

See [docs/troubleshooting.md](docs/troubleshooting.md) for the full list. Two issues come up often enough to call out here:

### Garbled output / wrong characters on non-US layouts

whisrs auto-detects your XKB layout via the active compositor (Hyprland / Sway), then `setxkbmap` (X11), then `localectl` (systemd), then the `XKB_DEFAULT_LAYOUT` / `XKB_DEFAULT_VARIANT` env vars — in that order. If none succeed, it falls back to US/QWERTY, and on a non-US layout that produces garbled output (e.g. `"this"` typed as `"èCDU"` on `fr(bepo)`).

To diagnose, run the daemon in the foreground with debug logging and look for the detected layout:

```bash
RUST_LOG=debug whisrsd
```

If the layout is missing or wrong, fix it one of two ways:

1. Make sure `localectl status` reports the right `X11 Layout` and `X11 Variant`. This is the system source-of-truth and works without any X session env vars.
2. Force the layout via env vars in your systemd service override:

   ```bash
   systemctl --user edit whisrs.service
   ```

   ```ini
   [Service]
   Environment=XKB_DEFAULT_LAYOUT=fr
   Environment=XKB_DEFAULT_VARIANT=bepo
   ```

   Then `systemctl --user restart whisrs.service`.

### Hotkey keys are physical positions, not layout characters

The configured hotkey trigger (e.g. `Ctrl+Shift+W`) is interpreted as the physical evdev keycode at the US/QWERTY `W` position, regardless of the active layout. This is intentional — the hotkey listener reads raw evdev events before any XKB translation, which is how every evdev-based hotkey tool works (xremap, sxhkd --evdev). On non-US layouts, pick the trigger by its physical position on a QWERTY keyboard.

---

## Contributing

The biggest way to help right now:

1. **Test on your compositor** — Sway, i3, KDE, GNOME. Report what works and what doesn't.
2. **Test on your distro** — Ubuntu, Fedora, NixOS, etc. Build issues, missing deps, etc.
3. **Bug reports** — if text goes to the wrong window, characters get dropped, or audio doesn't capture, open an issue.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and project structure.

---

## [How whisrs Compares](docs/comparison.md)

## [FAQ](docs/faq.md)

---

## License

MIT
