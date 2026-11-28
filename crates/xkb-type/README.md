# xkb-type

Layout-aware text injection for Linux: types the right characters whatever keyboard layout the user has active.

## Problem

Linux has no "type this text at the cursor" API. A program that injects fixed keycodes, as a uinput device does, gets them read through the user's active layout, so a non-US layout produces wrong characters: German users get "z" and "y" swapped, French users "a" and "q", and Russian users gibberish.

## Backends

All three implement the `KeyInjector` trait, so a caller can pick one at runtime and keep the same typing code.

| Backend | Type | Needs | Feature |
|---|---|---|---|
| uinput | `Keyboard::new` | Write access to `/dev/uinput` | none (always built) |
| Wayland virtual keyboard | `wayland_vk::WaylandVkKeyboard::new` | A compositor that offers `zwp_virtual_keyboard_v1` | `wayland-vk` |
| X11 XTEST | `Keyboard::new_xtest` | An X11 session (not Xwayland) | `xtest` |

- **uinput** on GNOME Wayland first probes the whisrs Shell extension for acknowledged layout control. With the updated extension on GNOME 50, it reads the active and available XKB sources, compiles their complete keymaps (including variants, model and options), checks switching and restoration before typing, and selects the appropriate source as scripts change. Character lookup is restricted to the selected XKB group; punctuation and modifiers come from that keymap. The original source is restored after completion, a gate stop, or an error. An error after typing starts stops the operation without replaying the text; the error identifies the byte offset and whether the current character may have been partly sent. If capability checks fail before typing, and any temporary switch has been restored, it uses the existing detection path. Explicit `with_layout` / `with_components` keyboards and XTEST retain their existing behavior.
- **uinput fallback** reads the active XKB layout with `xkbcommon`, maps every character to the physical key and modifiers that produce it (AltGr and dead keys included), and types through a virtual keyboard device. Characters the layout lacks (emoji, rare symbols) are pasted through the clipboard (Ctrl+V), and the clipboard is restored afterwards.
- **Wayland virtual keyboard** uploads its own keymap to the compositor, so each character types correctly whatever layout is active, mixed scripts included. It needs no `/dev/uinput`. `new` fails when the compositor lacks the protocol, so a caller can fall back to uinput.
- **XTEST** sends keys through the X server, which processes them in order, so a key delay of zero drops nothing. It needs no `/dev/uinput`, and refuses to start on Xwayland or when the server's keyboard layout differs from the detected one.

## Usage

```rust
use xkb_type::{Keyboard, Key};
use std::time::Duration;

// uinput, with the detected layout and clipboard backend.
let mut kb = Keyboard::new(Duration::from_millis(2))?;
kb.type_text("Hello, こんにちは, €100, 😀")?;
kb.backspace(5)?;
kb.send_combo(&[Key::KEY_LEFTCTRL, Key::KEY_C])?;
```

To force a specific XKB layout (skips the compositor probe):

```rust
use xkb_type::Keyboard;
use std::time::Duration;

let mut kb = Keyboard::with_layout("de", None, Duration::from_millis(5))?;
kb.type_text("Schöne Grüße")?;
```

To inject your own keymap and clipboard backend:

```rust
use xkb_type::{Keyboard, XkbKeymap, KeyboardLayout, default_clipboard};
use std::time::Duration;

let layout = KeyboardLayout::detect();
let keymap = XkbKeymap::from_layout(&layout)?;
let mut kb = Keyboard::with_components(keymap, default_clipboard(), Duration::from_millis(5))?;
```

To prefer the Wayland virtual keyboard and fall back to uinput (feature `wayland-vk`):

```rust
use std::time::Duration;
use xkb_type::{wayland_vk::WaylandVkKeyboard, KeyInjector, Keyboard};

let delay = Duration::from_millis(2);
let kb: Box<dyn KeyInjector> = match WaylandVkKeyboard::new(delay) {
    Ok(vk) => Box::new(vk),
    Err(_) => Box::new(Keyboard::new(delay)?),
};
```

To pause or stop between keys, type through the `KeyInjector` trait. Its `type_text` asks a gate before every key with the byte offset of the next character; the gate may block, and `ControlFlow::Break` stops before that character:

```rust
use std::ops::ControlFlow;
use std::time::Duration;
use xkb_type::{KeyInjector, Keyboard, Typed};

let mut kb: Box<dyn KeyInjector> = Box::new(Keyboard::new(Duration::from_millis(2))?);
let text = "hello world";
let mut gate = |at: usize| if at < 5 { ControlFlow::Continue(()) } else { ControlFlow::Break(()) };
if let Typed::StoppedAt(at) = kb.type_text(text, &mut gate)? {
    println!("not typed: {:?}", &text[at..]);
}
```

## Clipboard

`default_clipboard()` picks the backend for the session: `WaylandClipboard` when `WAYLAND_DISPLAY` is set, which runs `wl-copy` and `wl-paste` (from `wl-clipboard`), else `X11Clipboard`, which needs the `arboard` feature. `NoopClipboard` does nothing. The `ClipboardBackend` trait also reads the primary selection.

## Features

| Feature | Adds |
|---|---|
| `arboard` | `X11Clipboard` through the `arboard` crate (without it, every call returns an error) |
| `logging` | Debug and warning messages through the `log` crate |

The backend features, `wayland-vk` and `xtest`, are in the [Backends](#backends) table. None is on by default.

## Requirements

- Linux, Rust 1.74 or newer
- `libxkbcommon` (installed on most Linux desktops)
- The backend's own needs, from the table above
- `wl-clipboard` for the Wayland clipboard
- For automatic source switching on GNOME Wayland: GNOME 50, `gdbus` (GLib tools), and the updated [whisrs Shell extension](../../contrib/gnome-shell-extension/README.md). Older extensions, unsupported Shell APIs, IBus sources, and externally supplied or locked keymaps use the existing input path. This does not enable switching on other compositors.

The Shell bridge uses its source manager and waits for Mutter to apply each switch; it never writes `gsettings current` or enables `Shell.Eval`. uinput has no acknowledgement that an application consumed a key, so layout boundaries include a short drain delay. As with ordinary typing, keep the destination focused. Characters absent from every configured XKB source still use the clipboard; an IME is not driven through raw XKB key events.

## Installation

```sh
cargo add xkb-type
cargo add xkb-type --features wayland-vk,xtest,arboard   # with every backend
```

## License

MIT
