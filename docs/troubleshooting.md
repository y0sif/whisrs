# Troubleshooting

## /dev/uinput permission denied

Copy the udev rule and add yourself to the `input` group:

```bash
sudo install -m644 contrib/99-whisrs.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG input $USER
```

Log out and back in for the group change to take effect.

If you are already in the `input` group and still get permission denied, the
`uinput` module is probably not loaded (`ls /sys/class/misc/uinput` fails). The
kernel only autoloads it when `/dev/uinput` is opened, and the node is
`root:root 0600` until then, so a non-root open never gets that far. The rule's
`static_node=uinput` option fixes the node's permissions at boot. To fix it
without rebooting:

```bash
sudo modprobe uinput
```

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

If whisrs records from the wrong microphone, set `[audio] device` to the source you want. `whisrs config` lists the available sources, or use a name from `pactl list short sources`. Restart the daemon afterwards (`whisrs restart`) and check its log (`journalctl --user -u whisrs`) for the line saying which device the name resolved to. An unknown name falls back to the system default with a warning that lists the valid names. See [Choosing the microphone](configuration.md#choosing-the-microphone).

## Noisy room or quiet mic

whisrs measures the audio of every dictation, command and agent turn, and every recording track. Levels are in dBFS, decibels relative to full scale: 0 is the loudest a sample can be, and a normal speaking voice near the mic sits around -40.

- **"Skipped: the audio was silent (peak -82 dB). Is the mic muted?"** Only digital silence reached whisrs: a muted mic or the wrong device (see [No microphone detected](#no-microphone-detected)).
- **"... the audio was too quiet (speech at -68 dB)"** A voice was heard, but too faint to send (under -65 dBFS). Move closer or raise the mic gain, for example `wpctl set-volume @DEFAULT_SOURCE@ 80%`.
- **"... the audio was without speech (background at -48 dB)"** There was sound but no speech. If you did speak, the background is drowning you out: try the noise filter below. A streaming dictation that typed nothing says "Nothing typed: the audio was ..." the same way.
- **Recording warnings**, each at most once per recording: "too quiet" (speech under -55 dBFS: move closer or raise the gain), "clipping" (the mic hit its limit: lower the gain), "noisy" (speech less than 10 dB above the background: try the noise filter below), and "nothing has played on the system track yet" (in a call, check `[recording] system_device`). Each track's levels are kept in `recording.json` and the transcript's front matter ([Recording](configuration.md#recording)).

Each dictation, command and agent turn also logs its levels (`journalctl --user -u whisrs`): the speech level, the background, the SNR (how far the speech is above the background), the peak and the share of clipped samples. Speech recognition copes down to about 10 dB of SNR and gets worse below about 6 dB.

If the speech detector itself misjudges your mic, `[audio] vad = "rms"` brings back the plain loudness test of whisrs 0.1.31 ([Speech detection](configuration.md#speech-detection)).

### A noise filter with PipeWire

whisrs does not remove noise itself. PipeWire can, with its filter-chain module (it runs audio plugins on a source) and RNNoise, a noise suppressor made for voice:

1. Install the plugin. On Arch: `sudo pacman -S noise-suppression-for-voice`. It provides `librnnoise_ladspa`, a LADSPA plugin (the standard Linux audio plugin format).
2. Copy PipeWire's example config into place:

   ```bash
   mkdir -p ~/.config/pipewire/filter-chain.conf.d
   cp /usr/share/pipewire/filter-chain/source-rnnoise.conf ~/.config/pipewire/filter-chain.conf.d/
   ```

3. Start PipeWire's filter-chain service, which is what reads that folder: `systemctl --user enable --now filter-chain.service` (if it already runs, `systemctl --user restart filter-chain.service`).
4. A new source named "Noise Canceling source" appears. Set `[audio] device = "Noise Canceling source"` (for recordings, `[recording] mic_device`) and run `whisrs restart`.

The example's `"VAD Threshold (%)"` control (50) mutes what RNNoise does not take for voice; lower it if the starts of words get cut. GTCRN, a newer noise suppression model, also has an experimental LADSPA plugin.

## API key errors (401 Unauthorized)

Double-check your key is valid and not expired. Ensure the correct environment variable is set (`WHISRS_GROQ_API_KEY`, `WHISRS_DEEPGRAM_API_KEY`, or `WHISRS_OPENAI_API_KEY`), or that the key in `~/.config/whisrs/config.toml` is correct. Re-run `whisrs setup` to reconfigure.

## Config file fails to load

The daemon starts anyway, on built-in defaults, so the first sign is usually dictation working with none of your settings. It logs `Failed to parse config at ~/.config/whisrs/config.toml: ...`, names the line and column of the offending value, says it is using defaults, and sends a desktop notification with the same text.

The CLI does not fall back silently:

- `whisrs config` refuses and exits non-zero, printing that same parse error. Nothing is written, so the file is left exactly as it is.
- `whisrs setup` reports what it found and starts a fresh config. When the new file is written, the old one is saved beside it as `config.toml.bak` at mode 0600, so an `api_key` that exists only in the broken file is still recoverable.

So either fix the reported line by hand and restart the daemon, or let `whisrs setup` regenerate the file and copy what you need out of the `.bak`.

## whisrs cannot read the config at all

Permissions and ownership are a separate case. The error says `cannot read config at ...` instead of `cannot parse config at ...`, and both `whisrs config` and `whisrs setup` refuse before doing anything: writing the config reads it first, so no command can repair this for you. The usual cause is a config written by `sudo whisrs setup`, which leaves the file owned by root:

```bash
ls -l ~/.config/whisrs/config.toml
sudo chown $USER ~/.config/whisrs/config.toml
chmod 600 ~/.config/whisrs/config.toml
```

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
