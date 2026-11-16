# Troubleshooting

This page lists common whisrs problems by symptom, with the cause and the fix.

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
rule's `static_node=uinput` option fixes that from the next boot. To fix it
now:

```bash
sudo modprobe uinput
```

The rule also grants the `input` group an ACL on `/dev/uinput`, which matters when
another rule (e.g. brltty) sets one. That line calls `/usr/bin/setfacl` and is
skipped when the path does not exist, so on distributions that put `setfacl`
elsewhere (NixOS, Guix) rewrite it after copying:

```bash
command -v setfacl >/dev/null && sudo sed -i "s|/usr/bin/setfacl|$(command -v setfacl)|g" /etc/udev/rules.d/99-whisrs.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Keep the `command -v` guard: without `setfacl` installed, the substitution
breaks that line for good, even after you install `acl`.

The Nix package does this substitution at build time.

## No microphone detected

Verify your mic is recognized: `arecord -l`. If nothing shows up, make sure ALSA or PulseAudio/PipeWire is installed and your mic is not muted. On PipeWire systems, install `pipewire-alsa` for ALSA compatibility.

If whisrs records from the wrong microphone, set `[audio] device` as [Choosing the microphone](configuration.md#choosing-the-microphone) describes, then run `whisrs restart`.

## Noisy room or quiet mic

whisrs measures the audio of every dictation, command and agent turn, and every recording track. Levels are in dBFS, decibels relative to full scale: 0 is the loudest a sample can be, and a normal speaking voice near the mic sits around -40.

- **"Skipped: the audio was silent (peak -120 dB). Is the mic muted?"** Only digital silence reached whisrs: a muted mic or the wrong device (see [No microphone detected](#no-microphone-detected)). With `[audio] vad = "rms"`, "silent" means the whole recording stayed under about -50 dBFS, which a quiet room does too, so the muted-mic question then comes only when it was digital silence.
- **"... the audio was too quiet (speech at -68 dB)"** A voice was heard, but too faint to send (under -65 dBFS). Move closer or raise the mic gain, for example `wpctl set-volume @DEFAULT_SOURCE@ 80%`.
- **"... the audio was without speech (background at -48 dB)"** There was sound but no speech. If you did speak, either the background is drowning you out (try the noise filter below) or your voice was too faint or short even to count as too quiet: move closer or raise the gain.
- **Recording warnings**, each at most once per recording, once your mic has heard 30 seconds of speech:
  - "too quiet": your speech is under -55 dBFS. Move closer or raise the gain.
  - "clipping": the mic hit its limit. Lower the gain.
  - "noisy": your speech is less than 10 dB above the background. Try the noise filter below.
  - "nothing has played on the system track yet", after 2 minutes of your speech: in a call, check `[recording] system_device`.

  "Too quiet" and "noisy" judge only your own speech, what the mic records while nothing plays on the system track, so the echo of a lecture on your speakers is not judged as your voice. The transcript's front matter keeps each track's levels and warnings ([Recording](recording.md)).
- **Your soft voice is missing from a recording while a video played on the laptop speakers.** whisrs removes the speakers' echo from the mic track, and a soft voice under that echo goes with it. Use headphones, or speak up.

A streaming dictation that typed nothing because its audio held no speech says "Nothing typed: the audio was ..." with one of the reasons above. With the overlay on (`[general] overlay`), the dictation and `[[llm_commands]]` skip notifications and "Nothing typed" do not show; command mode and agent turns always show theirs.

Every skip is in the daemon log either way (`journalctl --user -u whisrs`). Each dictation, command and agent turn also logs its levels there (speech, background, SNR, peak and clipping) and when the speech detector heard something ("heard at 0.35 to 8.20 s", or "heard nothing"). SNR, the signal-to-noise ratio, is how far the speech is above the background in dB: speech recognition copes down to about 10 dB and gets worse below about 6 dB.

If the speech detector itself misjudges your mic, `[audio] vad = "rms"` switches to a plain loudness test ([Speech detection](configuration.md#speech-detection)).

### A noise filter with PipeWire

whisrs does not remove noise itself. PipeWire can, with its filter-chain module (it runs audio plugins on a source) and RNNoise, a noise suppressor made for voice:

1. Install the plugin. On Arch: `sudo pacman -S noise-suppression-for-voice`. It provides `librnnoise_ladspa`, a LADSPA plugin (an audio plugin format PipeWire's filter-chain can load).
2. Copy PipeWire's example config into place:

   ```bash
   mkdir -p ~/.config/pipewire/filter-chain.conf.d
   cp /usr/share/pipewire/filter-chain/source-rnnoise.conf ~/.config/pipewire/filter-chain.conf.d/
   ```

   It filters your default mic. To filter another one, add `target.object = "<source name>"` under `capture.props` in the copy, with a name from `pactl list short sources`.

3. Start PipeWire's filter-chain service, which is what reads that folder: `systemctl --user enable --now filter-chain.service` (if it already runs, `systemctl --user restart filter-chain.service`).
4. A new source named "Noise Canceling source" appears; `pactl list short sources` lists it as `effect_output.rnnoise`, and either name works. Set `[audio] device = "Noise Canceling source"` (a recording uses it too, unless `[recording] mic_device` is set) and run `whisrs restart`.

The example's `"VAD Threshold (%)"` control (50) mutes what RNNoise does not take for voice: lower it if the starts of words get cut. Above 0 it sends exact digital silence between your words, so a long call where you mostly listen can trip the recording's muted-mic alarm ("your mic has sent only silence for 3 minutes"). If that happens, set it to 0.

## API key errors (401 Unauthorized)

Check that the key is valid and not expired. A key variable in the daemon's environment (`WHISRS_GROQ_API_KEY`, `WHISRS_DEEPGRAM_API_KEY`, `WHISRS_OPENAI_API_KEY` or `WHISRS_ASR_SIDECAR_API_KEY`) wins over the key in `~/.config/whisrs/config.toml`, so a stale one hides a correct file ([Environment variables](configuration.md#environment-variables)). In `whisrs config`, `t` on a key row tests the key with one request; `whisrs setup` sets it again.

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

On GNOME and KDE whisrs cannot focus the window you started in again ([Terminal detection](configuration.md#terminal-detection) says why), so keep the target window focused until the text appears.

## TUI drops characters while whisrs types

Some terminal UIs drop characters when whisrs types fast: raise `[input] key_delay_ms` as [Typing](configuration.md#typing) describes, then run `whisrs restart`.

## Daemon not running

Start the daemon by hand (`whisrsd`) or through your service manager. `whisrs setup` installs the user service; to install it yourself under systemd, point `ExecStart` at the installed `whisrsd`, since systemd ignores your shell's `PATH` and the shipped unit names a bare `whisrsd`:

```bash
install -Dm644 contrib/whisrs.service ~/.config/systemd/user/whisrs.service
sed -i "s|^ExecStart=.*|ExecStart=$(command -v whisrsd)|" ~/.config/systemd/user/whisrs.service
systemctl --user enable --now whisrs.service
systemctl --user status whisrs.service
```

OpenRC user services need OpenRC 0.55 or newer:

```bash
install -Dm755 contrib/openrc/whisrs.initd ~/.config/rc/init.d/whisrs
install -Dm644 contrib/openrc/whisrs.confd ~/.config/rc/conf.d/whisrs
rc-update --user add whisrs default
rc-service --user whisrs start
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

The uinput backend types each character with the key that makes it in your XKB layout, so it needs to know that layout. whisrs takes it from Hyprland's keyboard, else `setxkbmap -query` (an X11 session), else `localectl status` (systemd). Only when none of those gives one does it read the `XKB_DEFAULT_LAYOUT` and `XKB_DEFAULT_VARIANT` variables, and with none at all it types as US/QWERTY, which garbles a non-US layout (e.g. `"this"` typed as `"èCDU"` on `fr(bepo)`). The Wayland virtual keyboard, which `[input] backend = "auto"` picks when the compositor has it, and `[input] paste = true` do not depend on the layout.

whisrs reads the layout when the daemon starts and does not log it, so run those commands yourself. Fix the first source that answers, then run `whisrs restart`:

1. On Hyprland, set `kb_layout` and `kb_variant` in its `input` section. On X11, `setxkbmap` with your layout.
2. Elsewhere, make `localectl status` report the right `X11 Layout` and `X11 Variant` (`localectl set-x11-keymap`). It needs no session variables.
3. When none of those answers (no `localectl` on an OpenRC system, say), set the variables on the service. They cannot override a layout found above.

   systemd (`systemctl --user edit whisrs.service`), then `systemctl --user restart whisrs.service`:

   ```ini
   [Service]
   Environment=XKB_DEFAULT_LAYOUT=fr
   Environment=XKB_DEFAULT_VARIANT=bepo
   ```

   OpenRC (`~/.config/rc/conf.d/whisrs`), then `rc-service --user whisrs restart`:

   ```sh
   XKB_DEFAULT_LAYOUT="fr"
   XKB_DEFAULT_VARIANT="bepo"
   ```

Typing also garbles when the focused window's active layout is not the one whisrs found, even when detection is right: per-window layouts on KDE (`SwitchMode=WinClass`, and KWin has no Wayland virtual keyboard), or a layout switched since the daemon started. `[input] paste = true` avoids it for batch dictation and command mode.

## Hotkey keys are physical positions, not layout characters

The configured hotkey trigger (e.g. `Ctrl+Shift+W`) is interpreted as the physical evdev keycode at the US/QWERTY `W` position, regardless of the active layout. This is intentional: the hotkey listener reads raw evdev events before any XKB translation, which is how every evdev-based hotkey tool works (xremap, sxhkd --evdev). On non-US layouts, pick the trigger by its physical position on a QWERTY keyboard.
