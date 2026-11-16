# How whisrs Compares

whisrs is a free, open-source voice tool built for the Linux desktop: dictation, read-aloud, voice-driven LLM commands and coding agents, from one daemon you bind to hotkeys. The table sets it beside other dictation tools, using what each vendor or project states about itself.

## Dictation tools side by side

| | whisrs | [Wispr Flow](https://wisprflow.ai/) | [Superwhisper](https://superwhisper.com/) | [Spokenly](https://spokenly.app/) | [nerd-dictation](https://github.com/ideasman42/nerd-dictation) | [Speech Note](https://github.com/mkiol/dsnote) |
|---|---|---|---|---|---|---|
| **Platforms** | Linux | macOS, Windows, iPhone, Android | macOS, Windows, iOS, Android, Linux | macOS, Windows, Linux, iPhone, Android | Linux | Linux, Sailfish OS |
| **On Linux** | Wayland and X11: Hyprland, Sway, Niri, GNOME, KDE, any X11 window manager | No native Linux app | Needs Wayland (Hyprland, KDE Plasma or GNOME) and systemd | X11 and Wayland | X11 by default; Wayland through ydotool, dotool or wtype | X11; Wayland needs the ydotool daemon |
| **Offline transcription** | whisper.cpp built in; Parakeet, Nemotron and others through local servers | No, dictation is transcribed in the cloud | Yes | Yes (Whisper, Parakeet) | Yes (VOSK) | Yes, and only offline (whisper.cpp, Vosk, Faster Whisper and more) |
| **Cloud transcription** | Groq, Deepgram, OpenAI, OpenRouter, with your own key | Yes | Yes | Its own cloud, or your own API keys | No | No |
| **Text reaches the app by** | Typed keystrokes, layout-aware (Wayland virtual keyboard, uinput or XTEST); paste optional | (no Linux app) | Paste | Needs `/dev/uinput` access to insert text | xdotool, ydotool, dotool or wtype | Insertion into the focused window |
| **Interface** | Daemon + CLI bound to any hotkey; TUI setup and config editor | Desktop and mobile apps | App; a hotkey, paste on release | App with a global hotkey | Script with begin and end commands to bind to keys | GUI app (Flatpak); global shortcuts on KDE Plasma and GNOME |
| **Price and license** | Free, open source (MIT), Rust | Free plan with a weekly word limit, paid plans | Free plan, paid Pro plan | Local models and own keys free; subscription for its cloud | Free, open source (GPL-3.0), Python | Free, open source (MPL-2.0), C++/Qt |

## What whisrs does on top of dictation

What whisrs does beyond dictation, from read-aloud to meeting transcription, is in the [README feature list](../README.md#features).
