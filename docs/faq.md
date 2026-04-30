# FAQ

**Does whisrs work on Wayland?**
Yes. whisrs has native support for Wayland compositors including Hyprland, Sway, GNOME, and KDE. It uses compositor-specific protocols for window tracking and uinput for keyboard injection, so it does not depend on X11 tools like xdotool.

**Can whisrs work offline?**
Yes. The local whisper.cpp backend runs transcription entirely on your machine. No API key, no internet connection, and no audio data leaves your device. Run `whisrs setup` and select the local backend to get started.

**What speech recognition backends does whisrs support?**
Six backends: Groq (cloud, free tier), Deepgram Nova REST and Streaming (cloud, 60+ languages, $200 free credit on signup), OpenAI REST (cloud), OpenAI Realtime (cloud, true streaming over WebSocket), and local whisper.cpp (offline, CPU/GPU). More local backends (Vosk, Parakeet) are planned.

**How does whisrs type text into applications?**
whisrs creates a virtual keyboard using Linux's uinput subsystem and performs XKB reverse lookups to find the correct keycode and modifier combination for each character. This means it respects your keyboard layout and works in any application, including terminals, editors, and browsers.

**Is whisrs a replacement for Wispr Flow or Superwhisper on Linux?**
Yes. Wispr Flow ships clients for macOS and Windows; Superwhisper is macOS-only. Neither has a Linux client. whisrs brings the same workflow to Linux: press a hotkey, speak, and text appears at your cursor. It supports both cloud and local transcription backends.

**What Linux distributions does whisrs support?**
whisrs works on any Linux distribution with the required system dependencies (alsa-lib, libxkbcommon, clang, cmake). It has been primarily tested on Arch Linux but also supports Debian/Ubuntu, Fedora, NixOS, and others. Install methods include AUR, cargo, Nix flake, and a universal install script.
