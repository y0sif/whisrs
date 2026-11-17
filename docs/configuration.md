# Configuration

whisrs reads `~/.config/whisrs/config.toml` (permissions `0600`). `whisrs setup` writes a
working one, and `whisrs config` edits every key in a full-screen editor. This page explains
how the features fit together.

The daemon reads the config only when it starts. After editing the file by hand, run
`whisrs restart`. `whisrs config` restarts the daemon after a save when it is idle (`r`
restarts it later).

## Full config reference

Every key, with its type and default, is in [config-reference.md](config-reference.md), one
table per section. A small config that dictates through Groq, with whisrs's own hotkeys,
command mode and read-aloud (every key left out takes its default):

```toml
[general]
backend = "groq"          # the default; see "Batch and streaming backends" below
language = "en"           # the default; "auto" detects it

[groq]
api_key = "gsk_..."       # example; or set WHISRS_GROQ_API_KEY and leave it out

[hotkeys]                 # no hotkey is bound by default
toggle = "Super+Shift+W"  # example: start and stop dictation
cancel = "Super+Shift+D"  # example

[llm]                     # command mode, [[llm_commands]], post-processing
api_key = "sk-..."        # example; the model defaults to gpt-4o-mini

[tts]
enabled = true            # read-aloud is off by default; groq is its default backend
```

## Batch and streaming backends

A streaming backend types text while you talk: `deepgram-streaming`, `openai-realtime`,
`openai-compatible-realtime`, and `local-whisper` (phrase by phrase). A batch backend sends
the audio when you stop and types the whole transcript at once: `groq`, `openai`,
`deepgram`, `asr-sidecar`, and `local-whisper` with `[local-whisper] segmentation = "none"`.
They differ in:

- **Auto-stop.** Streaming dictation stops after `[general] silence_timeout_ms` of silence
  once you spoke. Batch dictation stops on a second press. Command mode, `[[llm_commands]]`
  and agent turns auto-stop on every backend. A session in which you never speak records
  until the second press or `whisrs cancel`.
- **Post-processing and paste.** Streaming dictation ignores `[general] llm_post_process`
  and `[input] paste`: it never has a whole transcript to rewrite or paste, and `whisrsd`
  warns at startup. An `[[llm_commands]]` hotkey rewrites on any backend.
- **Held modifiers.** whisrs never types while Super, Alt, Ctrl or Shift is held, which it
  sees through `/dev/input` (the `input` group; without access it types at once). A batch
  dictation waits up to `[input] modifier_wait_ms`, then copies the rest to the clipboard; a
  streaming one waits for the release however long it takes (`whisrs cancel` ends the wait).
- **local-whisper with `segmentation = "none"`** types nothing until you stop, then waits
  for the whole dictation to decode (a few seconds per minute of speech on a GPU, longer on
  a CPU). Command mode and `[[llm_commands]]` always take this batch path.

## Hotkeys

Bind `whisrs toggle`, `whisrs cancel` and the other commands in your compositor, or let
whisrs listen for its own global hotkeys in `[hotkeys]`, `[[llm_commands]]` and `[[agents]]`.

- A hotkey is modifiers (`Super`, `Alt`, `Ctrl`, `Shift`) and a key: `A`-`Z`, `0`-`9`,
  `F1`-`F24`, `Space`, `Enter`, `Escape`, `Tab`, `Backspace`, `Delete`, `Insert`, `Home`,
  `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`, `ScrollLock` or `Pause`.
- At least one modifier is needed, except for `ScrollLock`, `Pause` and `F13`-`F24`, which
  fire alone only while no modifier is held. The modifiers must match exactly.
- Keys are not grabbed: the press still reaches the focused window. Two bindings on one
  combo both fire, and whisrs warns at startup. Keys are
  [physical positions](troubleshooting.md#hotkey-keys-are-physical-positions-not-layout-characters),
  not the characters of your layout.

## Typing

whisrs types through a virtual keyboard. `[input] backend = "auto"` uses the Wayland
virtual keyboard when the compositor has it, else uinput (`/dev/uinput`).

- `xtest` (X11 only, never picked by `auto`) types through the X server: no `/dev/uinput`
  access needed, and `key_delay_ms = 0` drops no keys. It refuses to start on Xwayland, when
  an XKB option moves Ctrl or Shift, and when the X server's keyboard layout differs from
  the one whisrs detected (run `setxkbmap` with your layout).
- `key_delay_ms` (default 2) is the pause between keys. Raise it to 4 to 10 when a terminal UI
  drops characters (some Ink-based ones, such as Claude Code, do); uinput treats 0 as 1.
- `paste = true` pastes with Ctrl+V (Ctrl+Shift+V in a terminal) instead of typing, which
  fixes garbled text on [non-US layouts](troubleshooting.md#garbled-output--wrong-characters-on-non-us-layouts).
  It replaces the clipboard for a moment and restores it after.
- `clipboard_fallback = true` also leaves the text in the clipboard (only the untyped rest
  when a held modifier stopped the typing). `clipboard_only = true` copies and never types
  or pastes, whatever the two keys above say. `whisrs cancel` never copies anything.

## Terminal detection

whisrs treats a focused terminal differently, so that a line break never runs a command
and copy and paste use the terminal's own shortcuts. In a terminal:

- command mode clears the prompt line with Ctrl+A then Ctrl+K before typing (wrong inside a
  TUI, and GNU screen takes Ctrl+A as its prefix);
- a multi-line LLM reply is refused and kept in `whisrs log` (see
  [What happens to the LLM's reply](#what-happens-to-the-llms-reply)), and a dictation's
  line breaks become spaces;
- the selection copy falls back to Ctrl+Shift+C after the primary selection (stock xterm and
  urxvt bind none, so there it copies nothing), and `paste = true` sends Ctrl+Shift+V.

With `clipboard_only = true`, only the copy shortcut applies. whisrs spots a terminal by the
focused window's class, and focuses the window you started in again before typing. Both need
the compositor to report the focused window, as Hyprland, Niri, Sway and X11 do; on GNOME and
KDE every window is an ordinary one, and text goes to the window focused when it is typed.

`[input] terminal_classes` adds classes to the built-in list, such as an `st` build with
its own `termname` or a dropdown window like `kitty-dropdown`. An entry matches the whole
class in any case, so `st` does not match `steam`. List only terminals: elsewhere command
mode's Ctrl+A, Ctrl+K empties the text field, and Ctrl+Shift+C opens a browser's DevTools.
Read the class with `hyprctl activewindow`, `niri msg focused-window`, `swaymsg -t get_tree`
(`app_id`, or `window_properties.class`) or `xprop WM_CLASS` (the second string); for one that
does not match, `RUST_LOG=debug whisrsd` logs an `is_terminal_class(...)` line with the verdict.

## LLM commands

Three features send text to the `[llm]` chat endpoint, which can be any OpenAI-compatible
`/chat/completions` URL, a local server included (LM Studio, Ollama, llama.cpp). whisrs
sends nothing without a key, so give a local server any placeholder, such as
`api_key = "not-needed"`.

- **Command mode** (`whisrs command`, `[hotkeys] command`) rewrites the selection:
  highlight some text, press the key and say what to do with it ("make this formal"). It
  reads the primary selection, else a simulated Ctrl+C, and refuses with "no text selected"
  when both come up empty.
- **`[[llm_commands]]`**: each entry has its own hotkey (or `whisrs llm-command <name>`).
  Dictate, the LLM applies the entry's `instruction`, and the result is typed.
- **`[general] llm_post_process = true`** runs every batch dictation through
  `llm_instruction`, a cleanup pass by default. If the LLM fails, times out (30 s) or
  returns nothing, the raw transcript is typed. `whisrs log` tags a rewritten one
  `<backend>+llm`.

An entry's `set_hotkey` (or `whisrs llm-command-set <name>`) makes the selected text its new
instruction, saved to the file and applied at once. Write an instruction that names the
dictated text ("the following text") and pins the output ("Return only ..., no
explanations"): small local models otherwise echo it or add comments.

```toml
[[llm_commands]]
name = "translate-de"
hotkey = "Super+Shift+T"
set_hotkey = "Super+Shift+Alt+T"
instruction = "Translate the following text into German. Return only the translation, with no explanations and no quotes."
```

### Generate text with no selection

Command mode needs a selection. To **write new text** where there is nothing to select, use
an `[[llm_commands]]` entry with a generic instruction. That path never reads the selection
or the clipboard: press the key, say what you want, and the result is typed at the cursor.

```toml
[[llm_commands]]
name = "ask"
hotkey = "Super+Shift+B"
instruction = "Treat the following text as a request and output only what is asked. Output the requested artifact itself, with no preamble, no explanation and no code fences."
```

Press `Super+Shift+B`, say "the command to install steam on arch linux", press again (or
stop talking), and `sudo pacman -S steam` is typed. From a compositor keybind, run
`whisrs llm-command ask` (Hyprland: `bind = $mainMod SHIFT, B, exec, whisrs llm-command ask`).

### What happens to the LLM's reply

The reply of `whisrs command` and every `[[llm_commands]]` entry is typed at the cursor:

- **A wrapping code fence is removed.** When the whole reply is one fenced block, the body
  is typed. A reply with several fenced blocks is left as it came. No key turns this off,
  so an instruction that asks for a fenced block cannot get one.
- **A multi-line reply is refused in a [terminal](#terminal-detection)**, where a line
  break is an Enter that would run a command. The text goes to the history, so `whisrs log`
  prints it (with `[general] history = false` it is discarded). The "not typed"
  notification shows even with `[general] notify = false`.
- The refusal holds with `[input] paste = true` too, since whisrs cannot see whether the
  program takes a bracketed paste. With `clipboard_only = true` the reply is copied, line
  breaks and all. Elsewhere, line breaks are kept.

Control characters such as Escape are removed from all text whisrs types, pastes or copies.

## ASR sidecar servers

The `asr-sidecar` backend posts to any server that speaks OpenAI's
`/v1/audio/transcriptions`, on your machine or hosted: LiteLLM, Speaches, or one you wrote.
What it sends is in the [sidecar contract](../contrib/asr-sidecars/README.md), beside local
recipes for Nemotron, Parakeet, Moonshine and VibeVoice. `api_key` is sent as a bearer
token, for a gateway that needs one.

### OpenRouter

[OpenRouter](https://openrouter.ai/) serves hosted speech-to-text models from several
providers behind one OpenAI-compatible endpoint, so `asr-sidecar` reaches them as they are:

```toml
[general]
backend = "asr-sidecar"

[asr-sidecar]
url = "https://openrouter.ai/api/v1/audio/transcriptions"
model = "microsoft/mai-transcribe-2"
api_key = "sk-or-..."   # or leave it out and set WHISRS_ASR_SIDECAR_API_KEY
```

| `model` | Note |
|---|---|
| `microsoft/mai-transcribe-2` | 60 languages |
| `elevenlabs/scribe-v2` | |
| `mistralai/voxtral-mini-transcribe-3-0` | compact |
| `nvidia/nemotron-3.5-transcribe` | open-weight |
| `qwen/qwen3-asr-1.7b` | 30 languages |
| `openai/whisper-large-v3-turbo` | what whisrs sends while `model` is unset |

- No speech-to-text model is free, and OpenRouter picks the provider, so a request can
  cost more than the list price. `whisrs setup` and `whisrs config` list every model with
  its price, and `t` there checks the key without sending audio.
- `[general] vocabulary` and `prompt` do nothing there: OpenRouter ignores `hotwords`, and
  whisrs warns when one is set.

Read-aloud reaches OpenRouter's voices through `tts-sidecar`:

```toml
[tts]
enabled = true
backend = "tts-sidecar"
url = "https://openrouter.ai/api/v1/audio/speech"
api_key = "sk-or-..."   # tts-sidecar sends no other key
```

Each model has its own voices, and while `voice` is unset whisrs sends the model's own. The
pickers in `whisrs config` list models with their voices and prices; any other can be typed.

### Realtime servers

`openai-compatible-realtime` streams to a server that speaks the OpenAI Realtime
transcription events over a WebSocket. Its `profile` names the server's dialect: `lemonade`
types completed phrases, and `llamacpp` types words as they arrive. A llama.cpp server
running Voxtral Realtime listens on `ws://localhost:<port>/v1/realtime`.

With `lemonade` and `turn_detection = "server-vad"`, the server ends a phrase at each
pause, so text appears as you talk; `"manual-commit"` sends one result when you stop.
`whisrs setup` fills in Lemonade's `url`, `ws://localhost:12345/realtime`; Lemonade must
load the model (not only `pull` it), or it returns empty text. NVIDIA's Nemotron 3.5 ASR
Streaming in [`nemo-speech`](../contrib/asr-sidecars/nemo-speech/) works with `lemonade`
too, run with `--endpointing` to type each phrase at a pause.

## Read aloud

`[tts] enabled = true` turns on `whisrs speak` (alias `read`), which reads the selected text
aloud; press it again to stop. `whisrs speak --text "..."` reads the given text
(`--text -` reads stdin), and agent replies are spoken the same way.

| `backend` | Default model and voice | Key |
|---|---|---|
| `groq` (default) | `canopylabs/orpheus-v1-english`, `autumn` | `[groq]` |
| `deepgram` | `aura-2-thalia-en` (the voice is in the model) | `[deepgram]` |
| `tts-sidecar` | `kokoro`, `af_heart` (a local server); on [OpenRouter](#openrouter), `hexgrad/kokoro-82m` | `[tts] api_key` only |
| `openai` | `gpt-4o-mini-tts`, `alloy`; stops working on 2027-01-06 | `[openai]` |

`[tts] api_key` wins over the backend's transcription key. `tts-sidecar` talks to any
OpenAI-compatible `/v1/audio/speech` server, by default `http://127.0.0.1:8880/v1/audio/speech`
(Kokoro-FastAPI). Audio cues and read-aloud play on the default output device.

### Read-aloud in Arabic

Groq's Orpheus also speaks Saudi Arabic. It needs a Groq key:
`[tts] api_key`, `[groq] api_key` or `WHISRS_GROQ_API_KEY`.

```toml
[tts]
enabled = true
backend = "groq"
model = "canopylabs/orpheus-arabic-saudi"
voice = "noura"
```

Its voices are `abdullah`, `fahad` and `sultan` (male) and `lulwa`, `noura` and `aisha`
(female), from [Groq's Orpheus docs](https://console.groq.com/docs/text-to-speech/orpheus).
Set `voice` with the model: the default, `autumn`, is an English voice.

The model needs a one-time step: an admin of your Groq organization accepts its terms at
<https://console.groq.com/playground?model=canopylabs%2Forpheus-arabic-saudi>. Until then
every request fails with a 400 `model_terms_required` error that holds that URL.

## Agent mode

Agent mode lets you talk to a coding agent (Claude Code, OpenCode, Codex or GitHub Copilot)
by voice: press an agent's hotkey, say what you want, and whisrs speaks its short reply
through `[tts]` when the turn ends. Setup, sessions and each harness are in [agents.md](agents.md).

## Choosing the microphone

`[audio] device` picks the input whisrs records from. The easiest way to choose it is the
Mic test screen (`whisrs config`, Esc to Home, Mic test): it shows each input's level live,
and `w` writes only `[audio] device`. By hand, the value is one of:

- `"default"` (or empty): the system default input, through ALSA. With the PipeWire or
  PulseAudio ALSA plugin (the usual setup) this follows the default source in your sound
  settings.
- A PulseAudio or PipeWire source name, such as
  `alsa_input.usb-Blue_Microphones_Yeti-00.analog-stereo`, from `pactl list short sources`
  (second column). The description your sound settings show works too (`wpctl status`
  lists them). A monitor source (a name ending in `.monitor`) records what your speakers
  play.
- An ALSA PCM name, such as `pipewire`, `plughw:1,0` or `sysdefault:CARD=PCH` (see
  `arecord -L`), also without a sound server.

Sound server sources are checked first, then ALSA names. The startup log names the device
picked; a name that matches nothing warns, lists the valid names and uses the default.

## Speech detection

`[audio] vad` decides what counts as speech: when auto-stop fires, whether a dictation,
command or agent turn is worth sending, where local-whisper cuts phrases, and where a
recording holds speech.

- `"earshot"` (the default): a small learned voice detector. A steady fan, a hum or a key
  click is not speech, and a quiet voice is. A noise that switches on (dishes, a fan
  cycling) can count for a moment, but alone it neither arms auto-stop nor gets a dictation
  sent.
- During dictation, command mode, `[[llm_commands]]` and agent turns, a voice far under
  yours (a TV, a colleague) does not count unless it is louder than about -50 dBFS. With a
  loud mic (your speech above about -32 dBFS), a TV in the room can still count. A recording
  counts every voice.
- `"rms"`: a plain loudness test (louder than about -50 dBFS is speech). Use it if the model
  handles your mic badly; in a noisy room it may never auto-stop.

Audio that is not sent is skipped with a notification that names the reason and the level.
[Noisy room or quiet mic](troubleshooting.md#noisy-room-or-quiet-mic) explains each reason,
the levels the daemon logs, and what to do.

## Recording

`whisrs recording` records your microphone and the system audio as two tracks, and after
the stop transcribes them into one timed Markdown transcript. How it works, speakers,
summaries and the MCP server are in [recording.md](recording.md).

## The vocabulary file

`[general] vocabulary` lists names and terms the backend should expect, and `prompt` gives
it free-form context, such as `"Speech is in English or Spanish. Transcribe in the language
spoken; never translate."`, which helps keep whisper from translating mixed-language speech.
Groq, OpenAI, local whisper, `asr-sidecar` and `openai-realtime` (with every model but
`gpt-realtime-whisper`) get both as one prompt. Deepgram takes the vocabulary as keyterms
(Nova-3 and Flux models only) and no prompt, and `openai-compatible-realtime` sends neither;
whisrs warns when a key goes to a backend that drops it.

Vocabulary terms can also live in `~/.config/whisrs/vocabulary.txt`, for setups where
`config.toml` is generated and read-only (Nix, chezmoi). Its path is fixed.

```
# One term per line. Blank lines and lines starting with # are skipped.
whisrs
Claude Code
NixOS
```

A term that starts with `#` is written with a leading backslash (`\#rust`); a `#` anywhere
else is literal, so `C# dev` needs nothing. At startup the file is merged into `[general]
vocabulary` (config.toml's terms first, duplicates dropped). A missing file is ignored.
When the file exists, `whisrs config` shows the merged list and writes a changed one to
`vocabulary.txt` whole (comments dropped), with an empty `vocabulary` in config.toml.

## Word replacements

`vocabulary` only hints the model. When it still mis-hears a term the same way every time,
map the wrong text to the right one:

```toml
[replacements]
"package build" = "PKGBUILD"
"whisper s" = "whisrs"
```

Every dictation is rewritten before it is typed, copied or logged, and so is every line of
a recording's transcript. Keys match in any case, on whole words, with any spaces or
hyphens between them, so "Package-build" matches too. Quote every key: unquoted,
`node.js = "Node.js"` is a nested table, config.toml fails to parse and the daemon falls
back to its defaults.

Punctuation around a match is kept ("package build." becomes "PKGBUILD."), and values are
inserted as written. An empty value deletes the match; leave filler words to
`remove_filler_words`. Command mode and `[[llm_commands]]` instructions are left alone, and
with `llm_post_process` the LLM sees the corrected text. A streaming backend is rewritten
one typed chunk at a time, so a pause inside a multi-word key can make it miss; on
`openai-realtime` a chunk can end inside a word, so a key like "pack" can match in "package".

## Profiles

A profile is a partial `config.toml` holding only what differs from it, in
`~/.config/whisrs/profiles/<name>.toml`. Say one setup dictates through Groq and another,
for a train with no network, runs offline:

```toml
# ~/.config/whisrs/profiles/offline.toml
[general]
backend = "local-whisper"   # the model in [local-whisper] model_path, ggml-base.en.bin by default
language = "en"
```

A name is 1 to 32 of `a-z`, `0-9`, `-` and `_`; `base`, `list` and `next` are taken. Tables
merge key by key, and any other value replaces the base one, so a profile's `vocabulary`
replaces the list. Every `api_key`, `[hotkeys]`, `[[llm_commands]]`, `[[agents]]` and
`[recording] dir` stay in `config.toml`: a profile that sets one has it ignored, with a
warning.

- `whisrs profile` lists `base` and the profiles (the active one `*`, a broken one with
  why), with the daemon running or not. `whisrs profile offline` switches, `base` goes back
  to `config.toml` alone, and `next` goes base, then the profiles by name, past any that
  does not load, as do `[hotkeys] next_profile` and the tray's Profile menu.
- The daemon refuses a switch while it dictates, reads aloud, records, processes a
  recording or runs an agent turn. Otherwise it checks the profile, notifies and restarts on
  it. With the daemon stopped, the name is written for its next start.

The active name is one line in `$XDG_STATE_HOME/whisrs/active-profile`; none, or `base`, is
`config.toml` alone. A profile that does not load never stops the daemon: it warns,
notifies and runs on `config.toml`, and `whisrs status` on a terminal says why.

## Hooks

`[hooks]` runs each time whisrs listens: dictation, command mode, `[[llm_commands]]` and
agent voice turns. `media_auto_pause = true` pauses the MPRIS players that are playing when
it starts and resumes exactly those when it stops, so media you paused yourself stays paused.
`on_record_start` and `on_record_stop` run a shell command without waiting for it, with the
daemon's environment, and `journalctl --user -t whisrsd -f` shows its output.

## Environment variables

`WHISRS_GROQ_API_KEY`, `WHISRS_DEEPGRAM_API_KEY`, `WHISRS_OPENAI_API_KEY` and
`WHISRS_ASR_SIDECAR_API_KEY` override the matching `api_key` in `config.toml`. With one set,
leave the `api_key` line out (keep the section if you set `model` there).
The `groq`, `openai` and `deepgram` read-aloud backends use them too, and `[llm]` uses
`WHISRS_OPENAI_API_KEY`, else `WHISRS_GROQ_API_KEY`, while its `api_key` is unset.

`RUST_LOG` sets the daemon's log level (`RUST_LOG=debug whisrsd`).

## Overlay

`[general] overlay = true` shows a small pill while you dictate, transcribe or hear text
read aloud, and the recording and transcribing notifications stay quiet (errors still show).
`[overlay]` sets its theme (`carbon`, `ember`, `cyan` or `custom`), size and place; with
`custom`, `[overlay.colors]` sets each colour (`#RGB`, `#RRGGBB` or `#RRGGBBAA`), and one
left unset is ember's. The agents' own overlay is in
[agents.md](agents.md#the-agent-overlay).

## GNOME overlay

GNOME on Wayland has no layer-shell protocol, which Hyprland and Sway use for the overlay,
so there the pill needs the bundled Shell extension:
[`contrib/gnome-shell-extension/README.md`](../contrib/gnome-shell-extension/README.md)
installs and updates it. `whisrs setup` does not replace an installed one, and an older one
ignores `[overlay] position`.
