# whisrs GNOME Shell overlay

This extension renders the whisrs recording overlay on GNOME Wayland. On GNOME 50 it also provides layout control for multilingual uinput typing, even when the recording overlay is disabled.
The daemon publishes recording state over the session D-Bus name
`org.whisrs.Overlay`; the extension listens for those state changes and draws
inside GNOME Shell.

Install locally:

```bash
mkdir -p ~/.local/share/gnome-shell/extensions
cp -r contrib/gnome-shell-extension/whisrs-overlay@eresende.github \
  ~/.local/share/gnome-shell/extensions/
gnome-extensions enable whisrs-overlay@eresende.github
```

Then set this in `~/.config/whisrs/config.toml`:

```toml
[general]
overlay = true
```

Restart the daemon:

```bash
systemctl --user restart whisrs.service
```

If GNOME does not load the extension immediately, log out and back in, then run
the `gnome-extensions enable` command again.

## Updating the extension

Copy the new version over the old one, delete `~/.cache/gnome-shell/extensions/whisrs-overlay@eresende.github`,
and log out and back in: GNOME Shell on Wayland cannot reload an extension within a session.

## Multilingual uinput typing

The updated extension exports `org.whisrs.Keyboard` at `/org/whisrs/Keyboard`
on the session bus name `org.gnome.Shell`. The `Snapshot` and `Select` methods
return JSON encoded as a byte array: active source ID and the XKB rules
(layouts, variants, group, model, options) for each configured source. `Select`
acknowledges a source only after Mutter has applied it. No arbitrary JavaScript
execution is exposed. Local session clients can read and select XKB sources.

This path requires GNOME 50's acknowledged keymap API and `gdbus` from GLib.
The daemon checks all required capabilities before typing and restores the
original source after typing, stopping, or an error. With an older extension,
an unsupported Shell API, IBus sources, or an external/locked keymap, it uses
the existing typing path. Wayland virtual keyboard and XTEST are unchanged.
See [xkb-type](../../crates/xkb-type/README.md) for injection behavior.

After updating, log out and back in as described above. To check that the new
API is loaded without changing the active source:

```bash
gdbus call --session --dest org.gnome.Shell \
  --object-path /org/whisrs/Keyboard --method org.whisrs.Keyboard.Snapshot
```
