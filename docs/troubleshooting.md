# Troubleshooting

## /dev/uinput permission denied

Copy the udev rule and add yourself to the `input` group:

```bash
sudo install -m644 contrib/99-whisrs.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG input $USER
```

Log out and back in for the group change to take effect.

The rule also grants the `input` group an ACL on `/dev/uinput`, which matters when
another rule (e.g. brltty) sets one — ACLs override plain group permissions. That
line is written against the FHS path `/usr/bin/setfacl` and is skipped when the
path does not exist, so on distributions that put `setfacl` elsewhere (NixOS,
Guix) rewrite it after copying:

```bash
command -v setfacl >/dev/null && sudo sed -i "s|/usr/bin/setfacl|$(command -v setfacl)|g" /etc/udev/rules.d/99-whisrs.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
```

The `command -v` guard matters: with `setfacl` not installed the substitution would
rewrite the rule to `TEST==""`, which stays dead even after you install `acl`.

The Nix package does this substitution at build time.

## No microphone detected

Verify your mic is recognized: `arecord -l`. If nothing shows up, make sure ALSA or PulseAudio/PipeWire is installed and your mic is not muted. On PipeWire systems, install `pipewire-alsa` for ALSA compatibility.

## API key errors (401 Unauthorized)

Double-check your key is valid and not expired. Ensure the correct environment variable is set (`WHISRS_GROQ_API_KEY`, `WHISRS_DEEPGRAM_API_KEY`, or `WHISRS_OPENAI_API_KEY`), or that the key in `~/.config/whisrs/config.toml` is correct. Re-run `whisrs setup` to reconfigure.

## Text goes to the wrong window

whisrs captures the focused window when recording starts and restores focus before typing. This requires compositor support. See the [Supported Environments](../README.md#supported-environments) table. On GNOME Wayland, the `window-calls` extension is required.

## TUI drops characters while whisrs types

Some Node/Ink-based terminal UIs (e.g. Claude Code in raw mode) can drop characters when whisrs injects text quickly. Raise the inter-key delay in `~/.config/whisrs/config.toml`:

```toml
[input]
key_delay_ms = 6   # default is 2; try 4–10 if characters get dropped
```

Restart the daemon for the change to take effect.

## Daemon not running

Start the daemon manually (`whisrsd`) or via systemd:

```bash
systemctl --user start whisrs.service
systemctl --user status whisrs.service
```

If it fails, check logs with `journalctl --user -u whisrs.service` or run `RUST_LOG=debug whisrsd` in the foreground.

## Model download fails (local whisper)

If automatic download during `whisrs setup` fails, download the model manually from HuggingFace:

```
https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin
```

Place it in `~/.local/share/whisrs/models/` and update `model_path` in your config.
