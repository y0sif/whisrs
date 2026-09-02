# Configuration

Config file: `~/.config/whisrs/config.toml` (permissions: `0600`).

The interactive `whisrs setup` will write a working file for you. The reference below documents every section.

## Full config reference

```toml
[general]
backend = "groq"            # groq | deepgram-streaming | deepgram | openai-realtime | openai | openai-compatible-realtime | local-whisper | asr-sidecar
language = "en"             # ISO 639-1 or "auto"
silence_timeout_ms = 2000   # auto-stop after silence (streaming only)
notify = true               # desktop notifications
remove_filler_words = true  # strip "um", "uh", "you know", etc.
filler_words = []           # custom list (empty = use built-in defaults)
audio_feedback = true       # play tones on record start/stop/done
audio_feedback_volume = 0.5 # 0.0 to 1.0
vocabulary = ["whisrs", "Hyprland"]  # custom terms for better transcription accuracy
                            # (sent to Deepgram as keyterms on Nova-3/Flux, ignored on
                            # older models; elsewhere it is folded into the prompt hint,
                            # so it only reaches the backends that send one — see prompt)
                            # very long lists are truncated for Deepgram, since every
                            # keyterm rides in the request URI — the daemon warns at
                            # startup naming how many terms actually reach it
                            # more terms can come from an optional vocabulary.txt next
                            # to this file — see "The vocabulary file" below
prompt = "Speech is in English or Spanish. Transcribe in the language spoken; never translate."
                            # optional sentence-style context, prepended to vocabulary
                            # (reaches Groq, OpenAI REST, local whisper.cpp, and the
                            # asr-sidecar, which takes it as its `hotwords` field.
                            # Deepgram sends no prompt — use vocabulary there instead.
                            # openai-compatible-realtime sends neither, so neither key
                            # reaches it. openai-realtime sends a prompt only on
                            # server-VAD models like gpt-4o-transcribe, not on the
                            # gpt-realtime-whisper that `whisrs setup` writes for it)
tray = true                 # system tray icon (requires SNI host like waybar)
overlay = false             # bottom-screen recording overlay (Hyprland/Sway, GNOME extension)

# Run every dictation through the [llm] backend before it is typed.
# Default: false. This is the always-on flavor of [[llm_commands]] below:
# same rewrite, but on the normal `whisrs toggle` key instead of a dedicated
# hotkey per entry. Needs an [llm] section (or WHISRS_OPENAI_API_KEY /
# WHISRS_GROQ_API_KEY).
#
# Batch backends only: deepgram, groq, openai, asr-sidecar. The streaming
# backends, which are deepgram-streaming, openai-realtime,
# openai-compatible-realtime AND local-whisper, type text as it arrives, so
# there is never a whole transcript to post-process and the flag does nothing
# at all. local-whisper is the one to watch: it runs offline and transcribes
# in a single call, but dictation with it always streams, so llm_post_process
# is a silent no-op there too. `whisrsd` warns at startup if you pair the two.
# Use an [[llm_commands]] hotkey instead, which works whatever the backend.
#
# If the LLM call fails, times out (30s), or returns nothing, the raw
# transcript is typed instead, so a dictation is never lost to post-processing.
# A post-processed dictation is logged as <backend>+llm (e.g. "groq+llm") in
# `whisrs log`; one that fell back to the raw transcript keeps the plain
# backend name.
llm_post_process = false
# Instruction applied to the transcript when llm_post_process is on. This is
# NOT `prompt` above: that one is a hint for the transcription backend and
# never reaches the LLM. Defaults to the conservative cleanup pass below, so
# `llm_post_process = true` alone already does something sensible.
llm_instruction = "Fix punctuation, capitalization and obvious transcription errors in the following text. Keep the wording and the meaning unchanged. Return only the corrected text, with no explanations and no quotes."

# Optional — controls overlay appearance when enabled.
# Defaults to a 100×40 pill with the "carbon" theme.
# When the overlay is on, recording/transcribing toast notifications are
# auto-suppressed (errors still pop) so the same event isn't double-signaled.
[overlay]
theme = "carbon"            # "carbon" (default) | "ember" | "cyan" | "custom"
width = 100                 # 90..=120 (clamped)
height = 40                 # 36..=48 (clamped)

# When theme = "custom", these override the named theme. Hex strings:
# #RGB, #RRGGBB, or #RRGGBBAA. Anything missing falls back to carbon.
# The transcribing color is reused for the read-aloud synthesizing sweep;
# speaking overrides the read-aloud audio-reactive bar color.
# [overlay.colors]
# background   = "#0E0E10EB"
# ring         = "#3A3A4050"
# recording    = "#F0EDF5"
# transcribing = "#9CA3AF"
# speaking     = "#5EEAD4"
# glow         = "#F0EDF5"

[audio]
device = "default"

[input]
# Inter-key delay for the virtual keyboard (uinput). Raise this if a TUI
# drops characters while whisrs is typing — e.g. Node/Ink-based apps like
# Claude Code in raw mode. Default: 2.
key_delay_ms = 2
# Inject text by clipboard paste (Ctrl+V) instead of typing keystrokes.
# Default: false.
#
# The uinput backend emits raw keycodes that the compositor decodes through
# the target window's ACTIVE XKB layout. On compositors without the Wayland
# virtual-keyboard protocol (e.g. KWin), when that active layout differs from
# the one whisrs detected — typically with per-window layouts (KDE
# SwitchMode=WinClass) or a non-US keymap — output is garbled (z<->y, mangled
# punctuation, dropped accents/umlauts). Pasting goes through the clipboard,
# which is layout-independent and Unicode-complete, so text comes out verbatim.
#
# Trade-offs: briefly replaces the clipboard (restored right after) and the
# target app must support Ctrl+V (terminals get Ctrl+Shift+V). It covers batch
# (non-streaming) dictation and command-mode output (`whisrs command` injects its
# LLM result with a single injection call, so it honors this whatever the
# backend).
# The streaming dictation path is the exception: streaming backends (including
# local-whisper, which always streams regardless of its `segmentation` mode)
# type incrementally and ignore it. `whisrsd` warns at startup if paste is set
# with one of those backends.
paste = false
# Leave the final transcript in the system clipboard in addition to
# injecting it at the cursor — a fallback if injection fails silently or
# produces garbled text, so you can paste and fix manually. Default: false.
#
# With paste mode (`paste = true`) the clipboard is normally restored to its
# previous content right after pasting; with clipboard_fallback set, that
# restore is skipped and the transcribed text stays in the clipboard. In
# typing mode (default) the text is copied to the clipboard after typing.
# Streaming backends (deepgram-streaming, openai-realtime, local-whisper, ...)
# type incrementally and copy the full final transcript when recording stops.
# `whisrs cancel` never copies anything, on any backend — cancel discards.
clipboard_fallback = false
# Copy-only mode: the final transcript is written to the clipboard and never
# injected at the cursor — no keystrokes, no Ctrl+V. Overrides paste and
# clipboard_fallback (both become no-ops). Handy for a "dictate to clipboard"
# workflow. Default: false.
clipboard_only = false
# Extra window classes to treat as terminal emulators, checked alongside the
# built-in list. Default: [] (built-in list only).
#
# WARNING: terminal detection is what makes command mode clear the prompt line
# with Ctrl+A then Ctrl+K before injecting its result. If you list a class that
# is not a terminal, that Ctrl+A / Ctrl+K goes into an ordinary text field and
# empties it. Terminal detection also swaps Ctrl+C for Ctrl+Shift+C in the
# selection path, so a mis-listed class can instead open DevTools in browsers
# and Electron apps. Only add classes you have confirmed belong to a terminal.
#
# The config is read once at daemon startup, so run `whisrs restart` after
# changing this key or the new value has no effect.
#
# To debug a class that will not match, run `RUST_LOG=debug whisrsd` and look
# for the `is_terminal_class(...)` line for that window. It names the class
# under test, the verdict, and (on a match) which stage matched: built-in
# whole identifier, your terminal_classes entry, or a built-in leaf name.
#
# Use this for the two cases the built-in list cannot cover:
#   * an st build with a custom `termname` in config.h, which reports that name
#     as its class instead of st-256color
#   * scratchpad / dropdown windows launched under a renamed class, such as
#     Alacritty-float, kitty-dropdown or wezterm-quake
#
# Entries are compared case-insensitively against the WHOLE window class, never
# as substrings, so "st" matches a window whose class is exactly "st" and not
# "steam". Unlike the built-in list, they are not matched by their last
# dot-segment either: listing "warp" matches the class "warp" and leaves
# "app.drey.Warp" (GNOME's Magic Wormhole client, not a terminal) alone. To
# match a reverse-DNS app_id, write it out in full.
#
# Read the class off your compositor:
#   hyprctl activewindow | grep class
#   niri msg focused-window
#   swaymsg -t get_tree   (the focused node's app_id for Wayland apps,
#                          window_properties.class for XWayland ones, falling
#                          back to window_properties.instance)
#   xprop WM_CLASS        (then click the window; the SECOND string is the
#                          class, and that is the one whisrs reports; when it
#                          is empty, whisrs falls back to the first string)
#
# A class is reported on Hyprland, Niri, Sway and X11. GNOME reports none
# without a shell extension (issue #72), and KDE reports none because whisrs
# does not speak the org_kde_plasma_window_management Wayland protocol (issue
# #127), so this key has no effect on either.
#
# Sway and X11 started reporting a class in issue #71, so terminal detection
# now fires on those sessions. Four behaviors change there:
#   * command mode clears the prompt line with Ctrl+A then Ctrl+K before
#     injecting. That is right at a shell prompt and wrong inside a TUI; GNU
#     screen takes Ctrl+A as its prefix, and many tmux users rebind theirs to
#     match.
#   * a multi-line LLM reply is refused at a terminal and kept in `whisrs log`
#     instead of being typed, including inside a terminal-hosted editor.
#   * the selection copy fallback sends Ctrl+Shift+C instead of Ctrl+C. Stock
#     xterm and urxvt do not bind Ctrl+Shift+C. The primary selection is tried
#     first and covers a highlighted selection, so this affects the fallback
#     only.
#   * with `paste = true`, injection sends Ctrl+Shift+V instead of Ctrl+V. This
#     one is a fix, not a loss: Ctrl+V at a terminal is readline quoted-insert.
terminal_classes = []

[groq]
api_key = "gsk_..."
model = "whisper-large-v3-turbo"

[deepgram]
api_key = "..."
model = "nova-3"

[openai]
api_key = "sk-..."
model = "gpt-4o-mini-transcribe"

# External OpenAI-compatible realtime server (WebSocket).
# Use this for Lemonade-style services that speak the OpenAI Realtime
# transcription event model over WebSocket instead of HTTP.
# In the current whisrs typing pipeline, replaceable interim partials are kept
# internal and only completed phrases are typed at the cursor.
# Note (Lemonade): load the model server-side (not just `pull`) or it returns empty text.
[openai-compatible-realtime]
url = "ws://localhost:12345/realtime"
model = "Whisper-Tiny"
profile = "lemonade"        # currently only "lemonade"
turn_detection = "server-vad"  # recommended; see notes below
# api_key = "optional bearer token"

# turn_detection:
# - "server-vad" (recommended): the server watches for pauses in your speech
#   and sends completed phrases as you talk. Choose this if you want text to
#   appear a phrase at a time during a longer dictation session.
# - "manual-commit": the server waits until whisrs stops recording before it
#   flushes the final phrase. Choose this if your server's pause detection is
#   too eager, or if you would rather get one result only when you stop.
#
# In both modes, whisrs only types completed text for this backend. It does
# not type unstable partial hypotheses as they change.

[local-whisper]
model_path = "~/.local/share/whisrs/models/ggml-base.en.bin"
# segmentation: how streaming audio is split before decoding.
# - "silence" (default): split into phrases at natural pauses and decode each
#   phrase exactly once. No overlap, no dedup — prevents repeated/invented
#   text. Continuous speech is force-split at 20 s so it still emits.
# - "window": legacy 8s/2s overlapping sliding window with text-based dedup.
# segmentation = "silence"
# phrase_silence_ms: continuous silence (ms) that ends a phrase in "silence"
# mode. Lower = snappier output, higher = fewer mid-sentence splits.
# phrase_silence_ms = 400

# Generic local ASR sidecar — talks to a small HTTP service that hosts the
# model (Moonshine, NVIDIA Parakeet, Microsoft VibeVoice-ASR, …). Keeps
# Python/PyTorch out of the Rust daemon. See contrib/asr-sidecars/ for
# ready-to-run sidecars and the wire-format contract.
#
# The contract is the OpenAI /v1/audio/transcriptions shape (multipart
# file/model/language in, {"text": ...} back), so this is also the backend for
# any OpenAI-compatible transcription endpoint — LiteLLM, Speaches, or a server
# you wrote yourself. The path is whatever you configure; /transcribe is only
# the default:
#   [asr-sidecar]
#   url = "http://127.0.0.1:8000/v1/audio/transcriptions"
#   model = "<model the endpoint serves>"
# One divergence from OpenAI: [general] prompt rides as the `hotwords` field
# (see prompt above), so a strict OpenAI-compatible server ignores it.
[asr-sidecar]
url = "http://127.0.0.1:8765/transcribe"
model = "microsoft/VibeVoice-ASR-HF"
# Optional bearer token. Only needed for endpoints that require auth (a hosted
# gateway, or a sidecar behind an authenticating proxy); local sidecars need
# none. WHISRS_ASR_SIDECAR_API_KEY overrides it.
# api_key = "optional bearer token"

# Command mode: LLM for voice-driven text rewriting.
# Also used by [[llm_commands]] (see below) and by
# [general] llm_post_process. Any OpenAI-compatible
# /chat/completions endpoint works here, including a local server:
#   [llm]
#   api_key = "not-needed"   # local servers don't validate this
#   model = "<model loaded in LM Studio / Ollama / llama.cpp server>"
#   api_url = "http://localhost:1234/v1/chat/completions"  # LM Studio default
[llm]
api_key = "sk-..."
model = "gpt-4o-mini"
api_url = "https://api.openai.com/v1/chat/completions"

# Named custom LLM commands: each gets its own hotkey. Dictate, the LLM
# applies the instruction to the transcribed text, result is typed at the
# cursor. A toggle-recording flavor of plain dictation — unlike command mode,
# there's no text selection involved. Uses the [llm] config above.
# Optional; can also be triggered via `whisrs llm-command <name>` for
# compositor keybind integration (same pattern as `whisrs toggle`).
# For one instruction applied to every dictation with no extra hotkey, use
# [general] llm_post_process instead.
#
# set_hotkey (optional): reprogram this command by selection. Highlight the new
# instruction text anywhere, press set_hotkey, and it becomes the command's
# instruction — saved back to this file and applied immediately (no restart).
# Lets you repurpose a command (translate -> summarize -> ...) without editing
# config. Also available as `whisrs llm-command-set <name>` for compositor
# binds. Selection-based on purpose: the instruction is exact, no dictation
# glitches.
#
# Tip: write the instruction so it clearly refers to the dictated text (e.g.
# "the following text") and pins the output ("Return only ..., no explanations,
# no quotes"). Small local models otherwise sometimes echo the instruction or
# add commentary.
[[llm_commands]]
name = "translate-de"
hotkey = "Super+Shift+T"           # run: dictate -> LLM -> type
set_hotkey = "Super+Shift+Alt+T"   # reprogram: select new instruction, press
instruction = "Translate the following text into German, using the friendly informal 'du' form and a warm, casual tone. Return only the translated text — no explanations, no quotes."

# Text-to-speech: read the current selection aloud (`whisrs speak`).
# Opt-in. model/voice are optional; each backend has its own default,
# so switching `backend` works without re-editing model/voice.
[tts]
enabled = false             # off by default
backend = "groq"            # groq | openai | deepgram | tts-sidecar
# model = "..."             # optional; backend default when unset.
#                           #   groq: canopylabs/orpheus-v1-english, openai: gpt-4o-mini-tts,
#                           #   deepgram: aura-2-thalia-en, tts-sidecar: kokoro
# voice = "..."             # optional; backend default (groq: autumn, openai: alloy,
#                           #   tts-sidecar: af_heart). Ignored by deepgram (voice is in the model id).
response_format = "wav"     # audio format requested from the API
# api_key = "..."           # optional; falls back to the backend's transcription key
#                           #   ([groq]/[openai]/[deepgram]). tts-sidecar needs none.
# url = "http://127.0.0.1:8880/v1/audio/speech"  # tts-sidecar only: local
#                           #   OpenAI-compatible server (Kokoro, Supertonic, ...)

# Built-in global hotkeys (optional, works without WM keybinds)
# Triggers: A-Z, 0-9, F1-F24, space, enter, escape, tab, backspace, delete,
#   insert, home, end, pageup, pagedown, up, down, left, right.
# Modifiers: Super, Alt, Ctrl, Shift. At least one is required, so a bare
#   "F13" is rejected; write "Shift+F13". The modifier set must match exactly,
#   so "Ctrl+Alt+Ins" does not fire while Shift is also held.
# Two bindings sharing a combo both fire on one press, so whisrs warns at
#   startup when it finds a duplicate across [hotkeys] and [[llm_commands]].
[hotkeys]
toggle = "Super+Shift+W"
cancel = "Super+Shift+D"
command = "Super+Shift+G"
speak = "Super+Shift+R"

# Recording-lifecycle hooks: pause playing MPRIS media while dictating, run
# shell commands fire-and-forget on record start/stop.  The child inherits the
# daemon's environment and stdout/stderr (journal under systemd --user).
[hooks]
media_auto_pause = true        # pause MPRIS players that are playing, resume those on stop
# on_record_start = ""         # shell command on recording start
# on_record_stop = ""          # shell command on recording stop
```

`media_auto_pause` only touches players that report `PlaybackStatus = "Playing"`
when recording starts, and resumes exactly those when it stops. A tab you paused
yourself stays paused. If a player's bus name changes mid-session (a browser
switching media sessions) its resume is skipped rather than guessed at.

To see what a hook printed, filter the journal by identifier, not by unit:

```fish
journalctl --user -t whisrsd -f
```

`journalctl --user -u whisrs` will usually **not** show it. Hook output comes from
the short-lived `sh` child, and journald resolves the owning unit from the sender's
PID — by the time it looks, a command like `echo hi` has already exited, so the
line lands in the journal without the unit attached and the `-u` filter drops it.

### Generate text with no selection

`whisrs command` **rewrites a selection**: highlight some text, press the key,
say what to do with it ("make this formal", "translate to German"). It reads
the primary selection, falling back to a simulated Ctrl+C, and refuses with
"no text selected" when it comes up empty.

To **write new text** where there is nothing to select, use an `[[llm_commands]]`
entry with a generic instruction. That path never reads the selection or the
clipboard, so nothing needs to be highlighted first: press the key, say what you
want, and the result is typed at the cursor.

```toml
[[llm_commands]]
name = "ask"
hotkey = "Super+Shift+B"
instruction = "Treat the following text as a request and output only what is asked. Output the requested artifact itself, with no preamble, no explanation and no code fences."
```

Then press `Super+Shift+B`, say "the command to install steam on arch linux",
press again (or stop talking), and `sudo pacman -S steam` is typed where your
cursor is. Say "a polite email declining the meeting on Thursday" and you get
the email. The instruction is what makes it generic, so one entry covers every
ad-hoc request; a second entry with a narrower instruction ("Translate the
following text into German. Return only the translation.") stays a dedicated
command.

It also works from a compositor keybind:
`bind = $mainMod SHIFT, B, exec, whisrs llm-command ask` on Hyprland,
`bindsym $mod+Shift+b exec whisrs llm-command ask` on Sway.

### What happens to the LLM's reply

The following applies to `whisrs command` and to every `[[llm_commands]]` entry,
since both type the model's reply at the cursor.

- **A wrapping code fence is removed.** If the whole reply is one fenced block,
  the fence goes and the body is typed. A reply containing several fenced blocks
  is prose about code, not a wrapper, and is left as it came.
  There is no opt-out, and that is a real limitation: an `[[llm_commands]]` entry
  whose instruction genuinely asks for a fenced block ("wrap this in a python
  code fence") cannot produce one — the fence is stripped on the way to the
  cursor, and no config key turns that off. If you need the fence characters
  themselves, type them yourself around the result.
- **Multi-line replies are typed normally**, except into a terminal. Translating
  a paragraph or drafting an email is exactly what these commands are for, so
  the line breaks are kept.
- **A multi-line reply is refused when the focused window is a terminal.** There
  a line break is an Enter, which would run a command before you have read it.
  The text is not lost: it goes to the history, so `whisrs log` prints it and you
  can copy it from there. Ask for a one-liner to get an injected result.
  You are told when this happens: the "not typed" notification fires even with
  `[general] notify = false`, because that setting means "don't narrate normal
  operation", not "discard my dictation quietly".

The refusal also applies with `[input] paste = true`, where the risk is lower
but not gone. Pasting into a terminal sends Ctrl+Shift+V, and a terminal with
bracketed paste enabled inserts multi-line text literally instead of running
each line — but bracketed paste is the foreground program's choice, not the
terminal's, and it is off inside many TUI programs and in some readline modes.
whisrs cannot see which is the case from the window class alone, and refusing
wrongly costs you one `whisrs log` lookup where injecting wrongly runs commands
you never read, so it refuses either way.

Terminal detection needs the compositor to report the focused window class,
which today means Hyprland, Niri, Sway and X11. On KDE and GNOME a terminal is
treated as an ordinary target, so a multi-line reply is typed there. Add any
class the built-in list misses to `[input] terminal_classes`.

## The vocabulary file

Vocabulary terms can also live in `~/.config/whisrs/vocabulary.txt`, next to
`config.toml`. This exists for setups where `config.toml` is generated and
read-only (Nix, chezmoi, any templated dotfiles), where adding one proper noun
to `[general] vocabulary` would otherwise mean a rebuild. The path is fixed —
there is deliberately no config key naming it, since setting one would need
the very `config.toml` edit the file avoids.

```
# One term per line. Blank lines and lines starting with # are skipped.
whisrs
Claude Code
NixOS
```

A term that starts with `#` is stored with a single leading backslash
(`\#rust`), which is stripped when the file is read; a `#` anywhere else on the
line is literal, so `C# dev` needs nothing.

At daemon startup the file is merged into `[general] vocabulary` — config.toml's
terms first, then the file's, duplicates dropped — before the config is
validated, so the Deepgram keyterm limits and their startup warnings count the
real list. A missing file is simply ignored; the feature is opt-in by creating
it. Like the rest of the config, the file is read once at startup, so run
`whisrs restart` after editing it.

When the file exists, the `whisrs config` vocabulary editor shows the merged
list. If you change that list, the whole of it is written back to
`vocabulary.txt` on save and config.toml is saved with an empty `vocabulary`,
so no term is stored twice. That rewrite is flat, so any comment lines and
blank-line grouping you added by hand are dropped at that point. If you leave
the vocabulary alone — including opening the editor and accepting the list as
shown — saving other settings touches neither store: your `vocabulary.txt`
keeps its comments and config.toml keeps its terms. Only
`vocabulary` gets this treatment; `prompt` and everything else stay in
`config.toml`.

## Environment variables

The following variables override the matching `api_key` in `config.toml`:

- `WHISRS_GROQ_API_KEY`
- `WHISRS_DEEPGRAM_API_KEY`
- `WHISRS_OPENAI_API_KEY`
- `WHISRS_ASR_SIDECAR_API_KEY`

With one of these set, the matching `api_key` line can be left out of `config.toml`
entirely. Keep the section itself if you set `model` there.

These provider keys are also used by the matching TTS backend (`groq`/`openai`/`deepgram`) unless `[tts] api_key` is set. The `tts-sidecar` backend needs no key.

`RUST_LOG` controls daemon log verbosity (e.g. `RUST_LOG=debug whisrsd`).

## GNOME overlay

GNOME Wayland does not support the wlroots layer-shell protocol used by Hyprland and Sway. To use `overlay = true` on GNOME, install the bundled GNOME Shell extension — see [`contrib/gnome-shell-extension/README.md`](../contrib/gnome-shell-extension/README.md) for install and update instructions.
