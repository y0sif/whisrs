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

Speech-to-text for Wayland, X11, Hyprland, Sway, GNOME, and KDE. Press a hotkey, speak, and your words appear at the cursor. Works with any app, any window manager, any desktop environment. Supports cloud transcription (Groq, Deepgram, OpenAI) and fully offline local transcription via whisper.cpp. Fast, private, open source.

---

## Why whisrs?

Dictation tools like Wispr Flow and Superwhisper are not available on Linux. [xhisper](https://github.com/imaginalnika/xhisper) proved the concept works, but I kept running into limitations. whisrs takes that idea and rebuilds it in Rust as a single async process with native keyboard layout support, window tracking, and multiple transcription backends.

---

## Installation

### Quick install (any distro)

```bash
curl -sSL https://y0sif.github.io/whisrs/install.sh | bash
```

Or clone and run locally:

```bash
git clone https://github.com/y0sif/whisrs && cd whisrs && ./install.sh
```

The install script handles everything: detects your distro, installs system dependencies, builds the project, and runs interactive setup.

After install, **press your hotkey** to start recording, **press again** to stop. Text appears at your cursor.

<details>
<summary><b>Other install methods (pre-built binary, AUR, Cargo, Nix, manual)</b></summary>

### Pre-built binary (Linux x86_64)

Each tagged release publishes a tarball on [GitHub Releases](https://github.com/y0sif/whisrs/releases/latest) with both `whisrs` and `whisrsd` plus the contrib files (udev rule, systemd unit, man pages).

```bash
# Full build (cloud + local whisper.cpp)
curl -sSL -o whisrs.tar.gz https://github.com/y0sif/whisrs/releases/latest/download/whisrs-linux-x86_64.tar.gz

# Or the minimal build (cloud backends only — smaller, no whisper.cpp)
curl -sSL -o whisrs.tar.gz https://github.com/y0sif/whisrs/releases/latest/download/whisrs-linux-x86_64-minimal.tar.gz

tar xzf whisrs.tar.gz
sudo install -m755 whisrs whisrsd /usr/local/bin/
sudo install -m644 contrib/99-whisrs.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG input $USER   # log out / back in for the group change
whisrs setup
```

| Variant | Includes local whisper.cpp | Tarball |
|---|---|---|
| `whisrs-linux-x86_64.tar.gz` | yes | full build |
| `whisrs-linux-x86_64-minimal.tar.gz` | no (cloud backends only) | minimal build |

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
| **Local whisper.cpp** | Local (CPU/GPU) | Sliding window | Free | Privacy, offline use |

Groq is the default. For fully offline use, run `whisrs setup` and select **Local > whisper.cpp** — `base.en` (142 MB, ~388 MB RAM) is recommended; `tiny.en` (75 MB) for low-end hardware, `small.en` (466 MB) for higher accuracy.

---

## Configuration

Config file: `~/.config/whisrs/config.toml` — `whisrs setup` writes a working file. A minimal example:

```toml
[general]
backend = "groq"   # groq | deepgram-streaming | deepgram | openai-realtime | openai | local-whisper
language = "en"    # ISO 639-1 or "auto"
overlay = false    # bottom-screen recording overlay

[groq]
api_key = "gsk_..."
```

Env-var overrides: `WHISRS_GROQ_API_KEY`, `WHISRS_DEEPGRAM_API_KEY`, `WHISRS_OPENAI_API_KEY`.

For the full reference (overlay, `[input]`, `[llm]`, `[hotkeys]`, GNOME extension setup), see [docs/configuration.md](docs/configuration.md).

---

## CLI Commands

```
whisrs setup     # Interactive onboarding
whisrs toggle    # Start/stop recording
whisrs cancel    # Cancel recording, discard audio
whisrs status    # Query daemon state
whisrs command   # Command mode: select text + speak instruction → LLM rewrite
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
| **X11 (any WM)** | Tested by community on Ubuntu 24.04 (Xorg) |
| **GNOME Wayland** | Tested by community on Ubuntu 24.04 and Arch (mutter); overlay via the bundled [GNOME Shell extension](contrib/gnome-shell-extension/README.md) |
| **KDE Wayland** | Implemented via D-Bus; reports welcome |
| **Audio** | PipeWire, PulseAudio, ALSA (auto-detected via cpal) |
| **Distros** | Confirmed on Arch Linux and Ubuntu 24.04; any Linux with the system dependencies above |

> **Note:** whisrs is daily-driven on Hyprland (Arch Linux), with community confirmation on GNOME Wayland (Ubuntu 24.04 + Arch) and Xorg (Ubuntu 24.04). Sway, i3, and KDE reports are still wanted — if you use whisrs there, please open an issue with what works and what doesn't.

---

## Project Status

whisrs is functional and usable for daily dictation. Streaming transcription, command mode, multi-language support, system tray, OSD overlay, layout-aware injection (incl. AltGr + dead keys), and packaging for AUR / Nix / crates.io all ship today. Local Vosk and Parakeet backends are next.

Per-release details: [docs/version-roadmap.md](docs/version-roadmap.md).

---

## Troubleshooting

See [docs/troubleshooting.md](docs/troubleshooting.md).

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
