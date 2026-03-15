```
         __    _
  _    _| |__ |_|___ _ __ ___
 \ \//\ / '_ \| / __| '__/ __|
  \  /\ \ | | | \__ \ |  \__ \
   \/  \/|_| |_|_|___/_|  |___/

  speak. type. done.
```

# whisrs

**Linux-first voice-to-text dictation, written in Rust.**

Press a hotkey, speak, and your words appear at the cursor — in any app, any window manager, any desktop environment. Fast, private, open source.

![Demo placeholder](assets/demo.gif)
*Demo coming soon — record yourself dictating and see text stream at the cursor.*

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

### Build

```bash
# Dependencies (Arch Linux)
sudo pacman -S base-devel alsa-lib libxkbcommon

# Build (cloud backends only)
git clone https://github.com/y0sif/whisrs
cd whisrs
cargo build --release

# Build with local whisper.cpp (offline transcription)
sudo pacman -S clang cmake    # additional deps for whisper.cpp
cargo build --release --features local-whisper
```

### Setup

```bash
# Interactive setup — picks your backend, enters API key
./target/release/whisrs setup

# Or manually create ~/.config/whisrs/config.toml
```

### uinput Permissions

whisrs needs write access to `/dev/uinput`:

```bash
sudo cp contrib/99-whisrs.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger
# Log out and back in
```

### Run

```bash
# Start the daemon
./target/release/whisrsd &

# Or use the systemd service
cp contrib/whisrs.service ~/.config/systemd/user/
systemctl --user enable --now whisrs.service
```

### Bind a Hotkey

Add to your WM/DE config. Example for Hyprland:

```
bind = $mainMod, W, exec, /path/to/whisrs toggle
```

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

Run transcription entirely on your machine — no API key, no internet, no data leaves your device.

```bash
# Build with local support
cargo build --release --features local-whisper

# Run setup — select Local > whisper.cpp, pick a model, download automatically
./target/release/whisrs setup
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
- [ ] Packaging (AUR, Nix, static binaries)

---

## Contributing

The biggest way to help right now:

1. **Test on your compositor** — Sway, i3, KDE, GNOME. Report what works and what doesn't.
2. **Test on your distro** — Ubuntu, Fedora, NixOS, etc. Build issues, missing deps, etc.
3. **Bug reports** — if text goes to the wrong window, characters get dropped, or audio doesn't capture, open an issue.

---

## License

MIT
