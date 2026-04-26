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
<summary><b>Other install methods (AUR, Cargo, Nix, manual)</b></summary>

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

Groq is the default. Fast, free tier, good accuracy with `whisper-large-v3-turbo`.

Deepgram offers $200 in free credits on signup (no credit card required) and supports 60+ languages with the Nova-3 model. The streaming backend provides true real-time transcription over WebSocket.

OpenAI Realtime is the premium option: true streaming over WebSocket means text appears at your cursor while you're still speaking.

### Local whisper.cpp

Run transcription entirely on your machine. No API key, no internet, no data leaves your device. Included in every build.

```bash
whisrs setup   # select Local > whisper.cpp, pick a model, download automatically
```

| Model | Size | RAM | Speed (CPU) | Accuracy |
|---|---|---|---|---|
| tiny.en | 75 MB | ~273 MB | Real-time | Decent |
| base.en | 142 MB | ~388 MB | Real-time | Good (recommended) |
| small.en | 466 MB | ~852 MB | Borderline | Very good |

---

## Configuration

Config file: `~/.config/whisrs/config.toml`

```toml
[general]
backend = "groq"            # groq | deepgram-streaming | deepgram | openai-realtime | openai | local-whisper
language = "en"             # ISO 639-1 or "auto"
silence_timeout_ms = 2000   # auto-stop after silence (streaming only)
notify = true               # desktop notifications
remove_filler_words = true  # strip "um", "uh", "you know", etc.
filler_words = []           # custom list (empty = use built-in defaults)
audio_feedback = true       # play tones on record start/stop/done
audio_feedback_volume = 0.5 # 0.0 to 1.0
vocabulary = ["whisrs", "Hyprland"]  # custom terms for better transcription accuracy
tray = true                 # system tray icon (requires SNI host like waybar)

[audio]
device = "default"

[input]
# Inter-key delay for the virtual keyboard (uinput). Raise this if a TUI
# drops characters while whisrs is typing — e.g. Node/Ink-based apps like
# Claude Code in raw mode. Default: 2.
key_delay_ms = 2

[groq]
api_key = "gsk_..."
model = "whisper-large-v3-turbo"

[deepgram]
api_key = "..."
model = "nova-3"

[openai]
api_key = "sk-..."
model = "gpt-4o-mini-transcribe"

[local-whisper]
model_path = "~/.local/share/whisrs/models/ggml-base.en.bin"

# Command mode: LLM for voice-driven text rewriting
[llm]
api_key = "sk-..."
model = "gpt-4o-mini"
api_url = "https://api.openai.com/v1/chat/completions"

# Built-in global hotkeys (optional, works without WM keybinds)
[hotkeys]
toggle = "Super+Shift+W"
cancel = "Super+Shift+D"
command = "Super+Shift+G"
```

Environment variable overrides: `WHISRS_GROQ_API_KEY`, `WHISRS_DEEPGRAM_API_KEY`, `WHISRS_OPENAI_API_KEY`

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
| **Hyprland** | Tested, full support |
| **Sway / i3** | Implemented, needs community testing |
| **X11 (any WM)** | Implemented, needs community testing |
| **GNOME Wayland** | Limited, requires `window-calls` extension for window tracking |
| **KDE Wayland** | Implemented via D-Bus, needs community testing |
| **Audio** | PipeWire, PulseAudio, ALSA (auto-detected via cpal) |
| **Distros** | Any Linux with the system dependencies above |

> **Note:** whisrs has been primarily tested on **Hyprland (Arch Linux)**. Testing on other compositors and distros is a valuable contribution. If you run into issues, please open an issue.

---

## Project Status

whisrs is functional and usable for daily dictation. The core features work:

- [x] Daemon + CLI architecture
- [x] Audio capture and WAV encoding
- [x] Groq, Deepgram (REST + streaming), OpenAI REST, and OpenAI Realtime backends
- [x] Local whisper.cpp backend (sliding window, prompt conditioning, model download)
- [x] Layout-aware keyboard injection (uinput + XKB)
- [x] Wayland/X11 clipboard with save/restore
- [x] Window tracking (Hyprland, Sway, X11, GNOME, KDE)
- [x] Desktop notifications and audio feedback
- [x] Interactive setup with LLM provider selection
- [x] Filler word removal
- [x] Transcription history (`whisrs log`)
- [x] Multi-language support (18 languages + auto-detect)
- [x] Custom vocabulary for improved transcription accuracy
- [x] LLM command mode (select text + voice instruction → rewrite)
- [x] System tray indicator (idle/recording/transcribing)
- [x] Configurable global hotkeys via evdev
- [x] Packaging ([AUR](https://aur.archlinux.org/packages/whisrs-git), Nix flake, crates.io)
- [ ] Local Vosk backend
- [ ] Local Parakeet backend (NVIDIA)

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
