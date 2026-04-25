# whisrs GNOME Shell overlay

This extension renders the whisrs bottom recording overlay on GNOME Wayland.
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

On Wayland, GNOME Shell caches extension bytecode and there is no in-session
reload. After changing any file, clear the cache and log out:

```bash
rm -rf ~/.cache/gnome-shell/extensions/whisrs-overlay@eresende.github
```

Then log out and back in. For already-installed extensions where only
`stylesheet.css` changed, a disable/enable cycle is sometimes enough:

```bash
gnome-extensions disable whisrs-overlay@eresende.github
gnome-extensions enable whisrs-overlay@eresende.github
```

But for `extension.js` changes a full session restart is required.
