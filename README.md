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

**Linux-first voice-to-text dictation, written in Rust.**

Press a hotkey, speak, and your words appear at the cursor — in any app, any window manager, any desktop environment. Fast, private, open source.

---

## Why whisrs?

This project is directly inspired by [xhisper](https://github.com/imaginalnika/xhisper) — a solid tool that proved Linux dictation works. whisrs takes that concept and rebuilds it in Rust with a proper architecture, native layout support, window tracking, and multiple transcription backends.

### Landscape

| | whisrs | xhisper | Wispr Flow | nerd-dictation | Superwhisper |
|---|---|---|---|---|---|
| **Platform** | Linux | Linux | macOS, Windows, iOS, Android | Linux | macOS, Windows, iOS |
| **Language** | Rust | C + Shell | Proprietary | Python | Proprietary |
| **Transcription** | Groq, OpenAI, local whisper.cpp | Groq only | Cloud (proprietary) | Vosk (local) | Local Whisper + cloud |
| **Streaming** | Yes (OpenAI Realtime) | No | Yes | No | Yes |
| **Offline** | Yes (whisper.cpp) | No | No | Yes | Yes |
| **Open source** | Yes (MIT) | Yes | No | Yes (GPL) | No |
| **Price** | Free | Free | Free tier / $12/mo Pro | Free | $8.49/mo or $250 lifetime |

Also worth knowing about:
- **[Speech Note](https://github.com/mkiol/dsnote)** — Linux desktop app (Flatpak) with offline STT, TTS, and translation. Supports Vosk, whisper.cpp, Faster Whisper. GUI-focused, not a CLI tool.
- **[VoiceInk](https://github.com/Beingpax/VoiceInk)** — macOS only, local Whisper, open source (GPL). No Linux.

### What whisrs adds over xhisper

| | whisrs | xhisper |
|---|---|---|
| **Keyboard layout** | Automatic XKB reverse lookup — works natively on any layout | Hardcoded QWERTY keycodes — non-QWERTY requires an input-switch workaround (e.g. `--rightalt` to toggle OS layout to QWERTY) |
| **Window tracking** | Captures focused window on record start, restores focus before typing | None — text goes to whatever window is focused |
| **Typing** | Bulk text processing in one pass through uinput | Character-by-character dispatch from shell to daemon over socket |
| **Audio capture** | Direct PCM via cpal (no temp files, no subprocess) | Shells out to `pw-record` |
| **Audio backends** | PipeWire, PulseAudio, ALSA (auto-detected) | PipeWire only |
| **Clipboard** | Save/restore around paste operations | Uses wl-copy/xclip (no restore) |
| **Backends** | Groq, OpenAI Realtime, OpenAI REST, local whisper.cpp | Groq only |
| **Streaming** | OpenAI Realtime WebSocket (text as you speak) | Not supported |

Both projects use `/dev/uinput` for keyboard injection and `wl-copy` for Unicode clipboard paste. Both have a daemon for the uinput device. The architectural difference is that xhisper's orchestration (recording, API calls, text dispatch) happens in a bash script that chains together `pw-record`, `curl`, `jq`, and `ffmpeg`, while whisrs does everything in a single async Rust process.

### Performance

whisrs is noticeably faster at typing transcribed text:

- **Bulk typing** — whisrs processes the full transcription in one pass. xhisper's shell iterates character-by-character, dispatching each to the daemon individually over a socket.
- **Single process** — xhisper's bash script spawns `pw-record`, `curl`, `jq`, `ffmpeg`, and `xhispertool` as separate processes. whisrs handles audio, HTTP, and typing in one binary.
- **Direct audio** — cpal streams PCM into memory. No subprocess, no temp WAV files on disk.
- **Async** — tokio runtime handles audio capture, API calls, and text typing concurrently.

---

## Quick Start

### One-line install

```bash
git clone https://github.com/y0sif/whisrs && cd whisrs && ./install.sh
```

The install script handles everything:
1. Installs system dependencies (detects your distro)
2. Builds the project (all backends included — cloud and local)
3. Installs `whisrs` and `whisrsd` to `~/.cargo/bin/`
4. Runs interactive setup — pick your backend, enter API key or download a local model
5. Fixes `/dev/uinput` permissions (asks for sudo)
6. Installs and enables the systemd service
7. Adds a keybinding to your compositor (Hyprland/Sway auto-detected)

After install, **press your hotkey** to start recording, **press again** to stop. Text appears at your cursor.

Want to switch backends later? Just run `whisrs setup` again.

<details>
<summary><b>Manual install (step by step)</b></summary>

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

This builds everything — cloud backends and local whisper.cpp support are all included in a single binary.

#### 3. Setup

```bash
whisrs setup
```

The interactive setup will walk you through backend selection, API keys / model download, microphone test, uinput permissions, systemd service, and keybindings.

#### 4. Manual uinput permissions (if you skipped during setup)

```bash
sudo cp contrib/99-whisrs.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger
sudo usermod -aG input $USER
# Log out and back in
```

#### 5. Manual daemon start (if you skipped during setup)

```bash
# Foreground
whisrsd

# Background
whisrsd &

# Systemd (recommended)
cp contrib/whisrs.service ~/.config/systemd/user/
systemctl --user enable --now whisrs.service
```

#### 6. Bind a hotkey

Example for Hyprland (`~/.config/hypr/hyprland.conf`):
```
bind = $mainMod, W, exec, whisrs toggle
```

Example for Sway (`~/.config/sway/config`):
```
bindsym $mod+w exec whisrs toggle
```

</details>

Then: **press hotkey** to start recording, **press again** to stop and transcribe. Text appears at your cursor.

---

## Transcription Backends

| Backend | Type | Streaming | Cost | Best for |
|---|---|---|---|---|
| **Groq** | Cloud (HTTP POST) | Batch | Free tier available | Getting started, budget use |
| **OpenAI Realtime** | Cloud (WebSocket) | True streaming | Paid | Best UX — text as you speak |
| **OpenAI REST** | Cloud (HTTP POST) | Batch | Paid | Simple fallback |
| **Local whisper.cpp** | Local (CPU/GPU) | Pseudo (sliding window) | Free | Privacy, offline use |
| **Local Vosk** | Local (CPU) | True streaming | Free | Coming soon |
| **Local Parakeet** | Local (NVIDIA) | True streaming | Free | Coming soon |

Groq is the default — fast, free tier, good accuracy with `whisper-large-v3-turbo`.

OpenAI Realtime is the premium option — true streaming over WebSocket means text appears at your cursor while you're still speaking.

### Local whisper.cpp

Run transcription entirely on your machine — no API key, no internet, no data leaves your device. Local whisper support is included in every build — no special flags needed.

```bash
# Run setup — select Local > whisper.cpp, pick a model, download automatically
whisrs setup
```

Models are downloaded from HuggingFace during setup:

| Model | Size | RAM | Speed (CPU) | Accuracy |
|---|---|---|---|---|
| tiny.en | 75 MB | ~273 MB | Real-time | Decent |
| base.en | 142 MB | ~388 MB | Real-time | Good (recommended) |
| small.en | 466 MB | ~852 MB | Borderline | Very good |

Streaming works via a sliding window approach: audio is processed in overlapping 8-second windows with prompt conditioning for consistency.

---

## Configuration

Config file: `~/.config/whisrs/config.toml`

```toml
[general]
backend = "groq"            # groq | openai-realtime | openai | local-whisper
language = "en"             # ISO 639-1 or "auto"
silence_timeout_ms = 2000   # auto-stop after silence (streaming only)
notify = true               # desktop notifications

[audio]
device = "default"

[groq]
api_key = "gsk_..."
model = "whisper-large-v3-turbo"

[openai]
api_key = "sk-..."
model = "gpt-4o-mini-transcribe"

[local-whisper]
model_path = "~/.local/share/whisrs/models/ggml-base.en.bin"
```

Environment variable overrides: `WHISRS_GROQ_API_KEY`, `WHISRS_OPENAI_API_KEY`

---

## CLI Commands

```
whisrs setup     # Interactive onboarding
whisrs toggle    # Start/stop recording
whisrs cancel    # Cancel recording, discard audio
whisrs status    # Query daemon state
```

---

## Supported Environments

| Component | Support |
|---|---|
| **Hyprland** | Tested, full support |
| **Sway / i3** | Implemented, needs community testing |
| **X11 (any WM)** | Implemented, needs community testing |
| **GNOME Wayland** | Limited — requires `window-calls` extension for window tracking |
| **KDE Wayland** | Implemented via D-Bus, needs community testing |
| **Audio** | PipeWire, PulseAudio, ALSA (auto-detected via cpal) |
| **Distros** | Any Linux with the system dependencies above |

> **Note:** whisrs has been primarily tested on **Hyprland (Arch Linux)**. Testing on other compositors and distros is a valuable contribution — if you run into issues, please open an issue.

---

## How It Works

```
Hotkey press
    |
    v
whisrs toggle --> Unix socket --> whisrsd (daemon)
                                    |
                                    v
                              State: Idle -> Recording
                                    |
                              cpal captures audio (16kHz mono)
                                    |
Hotkey press again                  |
    |                               v
    v                         State: Recording -> Transcribing
whisrs toggle --> Unix socket       |
                                    v
                              Encode WAV -> Send to API -> Get text
                                    |
                                    v
                              Restore window focus (Hyprland IPC)
                                    |
                                    v
                              Type text via uinput (XKB layout-aware)
                                    |
                                    v
                              State: Transcribing -> Idle
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and project structure.

---

## Troubleshooting

- **/dev/uinput permission denied** -- Copy the udev rule and add yourself to the `input` group:
  ```bash
  sudo cp contrib/99-whisrs.rules /etc/udev/rules.d/
  sudo udevadm control --reload-rules && sudo udevadm trigger
  sudo usermod -aG input $USER
  ```
  Log out and back in for the group change to take effect.

- **No microphone detected** -- Verify your mic is recognized: `arecord -l`. If nothing shows up, make sure ALSA or PulseAudio/PipeWire is installed and your mic is not muted. On PipeWire systems, install `pipewire-alsa` for ALSA compatibility.

- **API key errors (401 Unauthorized)** -- Double-check your key is valid and not expired. Ensure the correct environment variable is set (`WHISRS_GROQ_API_KEY` or `WHISRS_OPENAI_API_KEY`), or that the key in `~/.config/whisrs/config.toml` is correct. Re-run `whisrs setup` to reconfigure.

- **Text goes to the wrong window** -- whisrs captures the focused window when recording starts and restores focus before typing. This requires compositor support. See the [Supported Environments](#supported-environments) table above. On GNOME Wayland, the `window-calls` extension is required.

- **Daemon not running** -- Start the daemon manually (`whisrsd`) or via systemd:
  ```bash
  systemctl --user start whisrs.service
  systemctl --user status whisrs.service
  ```
  If it fails, check logs with `journalctl --user -u whisrs.service` or run `RUST_LOG=debug whisrsd` in the foreground.

- **Model download fails (local whisper)** -- If automatic download during `whisrs setup` fails, download the model manually from HuggingFace:
  ```
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin
  ```
  Place it in `~/.local/share/whisrs/models/` and update `model_path` in your config.

---

## Project Status

whisrs is functional and usable for daily dictation on Hyprland. The core features work:

- [x] Daemon + CLI architecture
- [x] Audio capture and WAV encoding
- [x] Groq transcription backend
- [x] OpenAI REST transcription backend
- [x] OpenAI Realtime WebSocket backend (needs API key testing)
- [x] Layout-aware keyboard injection (uinput + XKB)
- [x] Wayland/X11 clipboard with save/restore
- [x] Window tracking (Hyprland, Sway, X11, GNOME, KDE)
- [x] Desktop notifications
- [x] Interactive setup
- [x] Error UX with actionable messages
- [x] Local whisper.cpp backend (sliding window streaming, prompt conditioning, model download)
- [ ] Local Vosk backend (true streaming, tiny model)
- [ ] Local Parakeet backend (NVIDIA, ultra-fast streaming)
- [ ] OpenAI Realtime end-to-end testing
- [ ] Multi-compositor testing
- [ ] Filler word removal
- [ ] LLM command mode
- [ ] System tray indicator
- [x] Packaging (AUR, Nix, static binaries)

---

## Contributing

The biggest way to help right now:

1. **Test on your compositor** — Sway, i3, KDE, GNOME. Report what works and what doesn't.
2. **Test on your distro** — Ubuntu, Fedora, NixOS, etc. Build issues, missing deps, etc.
3. **Bug reports** — if text goes to the wrong window, characters get dropped, or audio doesn't capture, open an issue.

---

## License

MIT
