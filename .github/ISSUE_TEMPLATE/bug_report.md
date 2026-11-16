---
name: Bug Report
about: Something isn't working as expected
title: ''
labels: bug
assignees: ''
---

## Description

A clear description of the bug.

## Steps to Reproduce

1. Start daemon with `whisrsd`
2. Run `whisrs toggle`
3. Speak for ~10 seconds
4. Run `whisrs toggle` again
5. ...

## Expected Behavior

What should have happened.

## Actual Behavior

What actually happened.

## Environment

- **Compositor:** (e.g., Hyprland 0.45, Sway 1.9, GNOME 46)
- **Distro:** (e.g., Arch Linux, Ubuntu 24.04)
- **Audio backend:** (PipeWire / PulseAudio / ALSA)
- **Keyboard layout:** (QWERTY / Dvorak / AZERTY / other)
- **Transcription backend:** (groq / deepgram / deepgram-streaming / openai / openai-realtime / openai-compatible-realtime / local-whisper / asr-sidecar)
- **whisrs version:** (`whisrs --version`)

## Daemon Logs

Paste the relevant lines of `journalctl --user -u whisrs` (under systemd), or run
`RUST_LOG=debug whisrsd` in a terminal and paste its output:

```
(paste logs here)
```
