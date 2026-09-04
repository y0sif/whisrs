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
another rule (e.g. brltty) sets one; ACLs override plain group permissions. That
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

Start the daemon manually (`whisrsd`) or via your service manager.

systemd:

```bash
systemctl --user start whisrs.service
systemctl --user status whisrs.service
```

OpenRC:

```bash
rc-service --user whisrs start
rc-service --user whisrs status
```

If it fails, check the logs: `journalctl --user -u whisrs.service` under systemd,
`$XDG_STATE_HOME/whisrs/whisrsd.log` (default `~/.local/state/whisrs/whisrsd.log`)
under OpenRC. Or run `RUST_LOG=debug whisrsd` in the foreground.

## No window tracking or clipboard paste under OpenRC

OpenRC runs services with a scrubbed environment and, unlike systemd, has no
user-environment store to import from. If `whisrsd` starts without
`WAYLAND_DISPLAY`, `HYPRLAND_INSTANCE_SIGNATURE` and `DBUS_SESSION_BUS_ADDRESS`,
window tracking, clipboard paste and the tray all fail.

`contrib/openrc/whisrs.initd` recovers these before starting the daemon. If you
wrote your own init script, either copy that logic or set `whisrsd_env_file` in
`~/.config/rc/conf.d/whisrs` to a file of `KEY='value'` lines.

Confirm what the daemon actually received:

```bash
tr '\0' '\n' < /proc/$(pgrep -x whisrsd)/environ | grep -E 'WAYLAND|DISPLAY|DBUS'
```

## Model download fails (local whisper)

If automatic download during `whisrs setup` fails, download the model manually from HuggingFace:

```
https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin
```

Place it in `~/.local/share/whisrs/models/` and update `model_path` in your config.

## Garbled output / wrong characters on non-US layouts

whisrs auto-detects your XKB layout via the active compositor (Hyprland / Sway), then `setxkbmap` (X11), then `localectl` (systemd), then the `XKB_DEFAULT_LAYOUT` / `XKB_DEFAULT_VARIANT` env vars, in that order. If none succeed, it falls back to US/QWERTY, and on a non-US layout that produces garbled output (e.g. `"this"` typed as `"èCDU"` on `fr(bepo)`).

To diagnose, run the daemon in the foreground with debug logging and look for the detected layout:

```bash
RUST_LOG=debug whisrsd
```

If the layout is missing or wrong, fix it one of two ways:

1. Make sure `localectl status` reports the right `X11 Layout` and `X11 Variant`. This is the system source-of-truth and works without any X session env vars.
2. Force the layout via env vars on the service.

   systemd (`systemctl --user edit whisrs.service`):

   ```ini
   [Service]
   Environment=XKB_DEFAULT_LAYOUT=fr
   Environment=XKB_DEFAULT_VARIANT=bepo
   ```

   Then `systemctl --user restart whisrs.service`.

   OpenRC (add to `~/.config/rc/conf.d/whisrs`):

   ```sh
   XKB_DEFAULT_LAYOUT="fr"
   XKB_DEFAULT_VARIANT="bepo"
   ```

   Then `rc-service --user whisrs restart`.

## Hotkey keys are physical positions, not layout characters

The configured hotkey trigger (e.g. `Ctrl+Shift+W`) is interpreted as the physical evdev keycode at the US/QWERTY `W` position, regardless of the active layout. This is intentional: the hotkey listener reads raw evdev events before any XKB translation, which is how every evdev-based hotkey tool works (xremap, sxhkd --evdev). On non-US layouts, pick the trigger by its physical position on a QWERTY keyboard.
