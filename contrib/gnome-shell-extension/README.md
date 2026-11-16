# whisrs GNOME Shell overlay

This extension renders the whisrs recording overlay on GNOME Wayland.
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
