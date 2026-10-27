# Configuration

Config file: `~/.config/whisrs/config.toml` (permissions: `0600`).

The interactive `whisrs setup` will write a working file for you. The reference below documents every section.

## Full config reference

```toml
[general]
backend = "groq"            # groq | deepgram-streaming | deepgram | openai-realtime | openai | openai-compatible-realtime | local-whisper | asr-sidecar
language = "en"             # ISO 639-1 or "auto"
silence_timeout_ms = 2000   # auto-stop after this much silence once you spoke: streaming
                            # dictation, and command mode, [[llm_commands]] and agent
                            # turns on every backend. 0 = off (stop with a second press)
notify = true               # desktop notifications
history = true              # save dictations to ~/.local/share/whisrs/history.jsonl for
                            # `whisrs log`. false stops new writes only: existing
                            # entries stay until `whisrs log --clear`
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
                            # reaches it. openai-realtime sends a prompt with
                            # gpt-live-transcribe and gpt-4o-transcribe, but not
                            # with gpt-realtime-whisper)
                            # `whisrsd` warns at startup if either key targets a backend
                            # that discards it. `whisrs config` warns the same on save.
                            # Recordings (whisrs recording) never send the prompt,
                            # only the vocabulary (and with local-whisper, neither).
                            # local-whisper sends it with every dictation phrase or
                            # window it decodes, so write it in the language you
                            # dictate in: a prompt in another language pulls the
                            # transcript toward that language.
tray = true                 # system tray icon (requires SNI host like waybar)
overlay = false             # on-screen dictation overlay (Hyprland/Sway, GNOME extension)

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
# at all. local-whisper is the one to watch: it runs offline, but dictation
# with it streams by default, so llm_post_process is a silent no-op there too.
# Set [local-whisper] segmentation = "none" to make it a batch backend.
# `whisrsd` warns at startup if you pair a streaming backend with this flag.
# Or use an [[llm_commands]] hotkey instead, which works whatever the backend.
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
# Where the pill sits: "bottom-center" (default) | "bottom-left" |
# "bottom-right" | "top-left" | "top-center" | "top-right".
# "-middle" works as a synonym for "-center". 16 px from the screen edges.
# On GNOME this needs the Shell extension from this release. An older
# one ignores the key and stays at the bottom center, and `whisrs setup`
# does not replace an installed extension, so update it by hand
# (contrib/gnome-shell-extension/README.md) and log out and back in.
position = "bottom-center"

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
device = "default"         # input device; see "Choosing the microphone" below
vad = "earshot"            # speech detector: "earshot" (a small learned model) or "rms"
                           # (the 0.1.31 loudness test); see "Speech detection" below

[input]
# Inter-key delay for the virtual keyboard. Raise this if a TUI
# drops characters while whisrs is typing — e.g. Node/Ink-based apps like
# Claude Code in raw mode. The uinput backend treats 0 as 1 (with no delay
# the kernel's input queue overflows and the end of the text is lost); the
# xtest backend honours 0. Default: 2.
key_delay_ms = 2
# Keyboard-injection backend: auto | uinput | wayland-vk | xtest.
# auto uses the Wayland virtual keyboard when the compositor supports it,
# otherwise uinput. Default: auto.
# On X11, set "xtest" to type through the X server's XTEST extension instead
# of uinput: no /dev/uinput access needed, and key_delay_ms = 0 drops no
# keys. auto never picks it. It refuses to start on Xwayland, and when the X
# server's XTEST keyboard has a different layout than the one whisrs detected
# (a layout set only in xorg.conf, e.g. by `localectl set-x11-keymap`, does
# not reach that keyboard; run setxkbmap with your layout to fix that), or
# when an XKB option moves Ctrl or Shift (e.g. ctrl:swapcaps; use auto).
backend = "auto"
# How long to wait, in milliseconds, for physically held modifier keys (Super,
# Alt, Ctrl, Shift) to be released before whisrs sends a keystroke with a
# batch backend (groq, openai, deepgram, local-whisper, asr-sidecar) or in
# command mode: typing, the paste shortcut, command mode's line clear and the
# selection copy. A key typed while a modifier is held becomes a shortcut, so
# stopping a dictation with Super+W and keeping Super down would otherwise
# fire Super+<letter> compositor binds. whisrs never types while a modifier
# is held, and checks before every key: pressing one while the text is being
# typed pauses the typing until it is released. If one is still down when
# this runs out, typing stops there: the rest of the text (all of it if
# nothing was typed yet) is copied to the clipboard and a warning is logged
# (paste it with Ctrl+V). Text already typed stays where it is. In command
# mode at a terminal, a modifier held from the start keeps the line from
# being cleared too. 0 = don't wait: if a modifier is held, the rest of the
# text goes straight to the clipboard.
# Streaming backends (deepgram-streaming, openai-realtime,
# openai-compatible-realtime) ignore this setting: they also pause before
# any key while a modifier is held, wait for the release however long it
# takes, and then type the waiting text in order. `whisrs cancel` ends that
# wait and discards the text not typed yet.
# The check reads /dev/input; when it can't, it can't see held keys and keys
# are sent as before. Default: 10000.
modifier_wait_ms = 10000
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
# local-whisper, unless its `segmentation` is "none") type incrementally and
# ignore it. `whisrsd` warns at startup if paste is set
# with one of those backends.
paste = false
# Leave the final transcript in the system clipboard in addition to
# injecting it at the cursor — a fallback if injection fails silently or
# produces garbled text, so you can paste and fix manually. Default: false.
#
# With paste mode (`paste = true`) the clipboard is normally restored to its
# previous content right after pasting; with clipboard_fallback set, that
# restore is skipped and the transcribed text stays in the clipboard. In
# typing mode (default) the text is copied to the clipboard after typing,
# except when a held modifier stopped the typing (see modifier_wait_ms): then
# the clipboard holds only the untyped rest, so pasting it does not repeat
# the typed part.
# Streaming backends (deepgram-streaming, openai-realtime, local-whisper, ...)
# type incrementally and copy the full final transcript when recording stops.
# `whisrs cancel` never copies anything, on any backend — cancel discards.
clipboard_fallback = false
# Copy-only mode: the final transcript is written to the clipboard and never
# injected at the cursor — no keystrokes, no Ctrl+V. Overrides paste and
# clipboard_fallback (both become no-ops). Handy for a "dictate to clipboard"
# workflow. Default: false.
#
# This also covers the reply of `whisrs command` and `[[llm_commands]]`. A
# multi-line reply is copied even with a terminal focused: nothing is typed,
# so it is not refused (see "What happens to the LLM's reply").
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
#     instead of being typed (discarded with `[general] history = false`),
#     including inside a terminal-hosted editor.
#   * the selection copy fallback sends Ctrl+Shift+C instead of Ctrl+C. Stock
#     xterm and urxvt do not bind Ctrl+Shift+C. The primary selection is tried
#     first and covers a highlighted selection, so this affects the fallback
#     only.
#   * with `paste = true`, injection sends Ctrl+Shift+V instead of Ctrl+V. This
#     one is a fix, not a loss: Ctrl+V at a terminal is readline quoted-insert.
# With `clipboard_only = true` only the third applies: nothing is typed or
# pasted, so there is no line clear, no refusal and no Ctrl+Shift+V.
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

# For backend = "openai-realtime", select model = "gpt-live-transcribe".
# languages = ["ru", "en"]  # expected input languages for mixed-language dictation

# gpt-live-transcribe streams text as you speak and commits the final transcript
# when recording stops. languages are hints, not an output-language guarantee.
# Precedence for this model: a per-session `whisrs toggle -l <code>`, then
# languages, then [general] language sent as a one-item list, then no hint when
# it is "auto". A -l equal to [general] language counts as no override and keeps
# the list. On any other model or backend, languages is ignored and `whisrsd`
# warns at startup. OpenAI also accepts some three-letter and regional codes.

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
#   text. Continuous speech is force-split at the first silent moment after
#   20 s (hard ceiling 28 s) so it still emits.
# - "window": legacy 8s/2s overlapping sliding window with text-based dedup.
# - "none": no streaming. Nothing is typed while you talk; when you stop,
#   the whole dictation is transcribed and typed at once. Use it for long
#   dictation you don't want split into phrases.
#   The wait after stopping grows with the dictation (a few seconds per
#   minute of speech with large-v3-turbo on a GPU, longer on CPU). It also
#   makes [general] llm_post_process and [input] paste apply to dictation.
#   Past 28 s the dictation is decoded in windows of at most 28 s, cut at
#   pauses (or at 28 s in continuous speech) and each given the prompt, so a
#   long dictation does not fall into whisper's repetition loop (issue #208).
#   Command mode and [[llm_commands]] always take this batch path, whatever
#   segmentation says. The recording feature (whisrs recording) ignores this
#   setting and always decodes in phrases.
# segmentation = "silence"
# phrase_silence_ms: continuous silence (ms) that ends a phrase in "silence"
# mode; "window" streaming ignores it. In every mode it also finds the pauses
# where a batch call over 28 s is cut ("none" dictation, command mode,
# [[llm_commands]]) and where a recording is cut, capped at 900 ms for both
# (for a recording, so every gap between the speech it sends ends a phrase).
# Lower = snappier output, higher = fewer mid-sentence splits.
# phrase_silence_ms = 400
# flash_attn: enable whisper.cpp flash attention (default false). It can cut
# decode time on GPU builds (cuda), most on long recordings, and may change
# the output slightly. Little to gain on CPU. Takes effect when the daemon
# loads the model, so restart it after changing this.
# flash_attn = false

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

# Text-to-speech: read the current selection aloud (`whisrs speak`), or the
# text you give it (`whisrs speak --text "..."`, `--text -` for stdin), and
# speak agent replies (see [[agents]] below).
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

# Agent mode (optional): talk to a coding agent by voice. Each entry is one
# agent working in one directory. Its replies are spoken through [tts] above
# (enable it), and shown as a notification when [general] notify is on. The
# daemon reads these entries when it starts: run `whisrs restart` after a
# change. See "Agent mode" below.
# [[agents]]
# name = "jarvis"                # required, unique: lowercase letters, digits, - and _
# hotkey = "Super+Shift+J"       # optional: records a voice turn, same syntax as [hotkeys]
# new_session_hotkey = "Super+Shift+N"  # optional: records a voice turn that starts a
#                                #   fresh session (whisrs agent <name> --new)
# harness = "claude"             # claude (Claude Code, default) | opencode (OpenCode 2) |
#                                #   codex (Codex CLI) | copilot (GitHub Copilot CLI)
# dir = "~/Projects/whisrs"      # required: the project the agent works in; must exist (a
#                                #   relative path is taken from whisrsd's working directory)
# permissions = "auto"           # auto (default) | bypass | ask. What auto allows:
#                                #   claude: Claude Code's auto mode (a classifier reviews each action)
#                                #   opencode: OpenCode's own rules (most tools allowed by default)
#                                #   codex: --approve-for-me (auto-approved inside the workspace-write sandbox)
#                                #   copilot: --allow-all-tools (every tool without asking, in dir)
# command = "claude"             # optional: the binary (default: claude, opencode, codex or
#                                #   copilot on PATH). Set its full path when the daemon's PATH lacks
#                                #   it (a systemd user service often has no ~/.local/bin)
# args = ["--model", "sonnet"]   # optional: extra arguments for the first turn of a session
#                                #   (Codex and Copilot: for every turn)
# reply_instruction = "..."      # optional: replaces the built-in voice instruction
#                                #   (after the restart, reply_instruction applies from the
#                                #   next new session, whisrs agent <name> --new, except with
#                                #   Copilot, which reads it on every turn; so do args, except
#                                #   with Codex and Copilot, which take them on every turn; and
#                                #   with Claude Code, permissions too)
# feedback = "brief"             # optional: what the agent says while it works. summary
#                                #   (the reply only) | brief (default: a short
#                                #   acknowledgement, then the reply) | conversational
#                                #   (also short updates at milestones). See "Feedback levels"

# The agent overlay (optional): a row per agent at work, in a screen corner of
# its own. Drawn only with an [[agents]] entry. See "The agent overlay" below.
# [agent_overlay]
# enabled = true                 # default: follows [general] overlay
# style = "orb"                  # orb (default) | face
# corner = "bottom-right"        # bottom-right (default) | bottom-left | top-right | top-left

# Recording (optional). Records your mic and the system audio as two tracks,
# then transcribes them after you stop. Every key is optional; see "Recording"
# below for how it works.
[recording]
# backend = "groq"            # default: [general] backend. A backend that can transcribe
#                             #   a recorded file: groq | openai | deepgram | asr-sidecar |
#                             #   local-whisper.
#                             #   deepgram-streaming uses Deepgram's REST API here.
#                             #   openai-realtime and openai-compatible-realtime are rejected.
# user_name = "Ada"           # your label in transcripts; default: your full name
#                             #   from the system account, else $USER
# language = "de"             # language of both tracks; default: [general] language.
#                             #   An ISO 639 code, optionally with a region, or "auto"
# mic_language = "de"         # language of your mic track; default: language
# system_language = "auto"    # language of the system audio track; default: language
# mic_device = "alsa_input.usb-..."  # a source name from whisrs config or pactl list short sources
#                             #   default: [audio] device
# system_device = "default"   # "default" = monitor of the default output,
#                             #   "none" = mic only, or a source name ending in .monitor
max_duration_minutes = 180    # recording stops by itself after this long (1 to 2160)
audio_retention_days = 7      # 0 = delete audio once transcribed, negative = keep forever
# dir = "~/Recordings"        # default: ~/.local/share/whisrs/recordings
# diarization = "auto"        # tell apart the voices on the system audio (see "Speakers"
#                             #   below): "auto" (the sidecar below when set, else
#                             #   Deepgram), "backend", "sidecar" or "off"
# diarization_url = "http://127.0.0.1:8770/diarize"  # a diarization sidecar (local server)
# num_speakers = 0            # voices the sidecar should find; 0 = let it decide (0 to 20)
# shared_mic = false          # several people share your mic: tell its voices apart too
# name_speakers = false       # ask the [llm] who the unnamed voices are; sends excerpts
#                             #   of each transcript to it (see "Naming the speakers")
# summary = false             # summarize each transcript with the [llm], and title a
#                             #   recording you gave no title; sends the whole transcript
#                             #   to it (see "Summaries and titles")
# summary_instruction = "..."  # your own rules for a summary, in place of the default ones

# Built-in global hotkeys (optional, works without WM keybinds)
# Triggers: A-Z, 0-9, F1-F24, space, enter, escape, tab, backspace, delete,
#   insert, home, end, pageup, pagedown, up, down, left, right, scrolllock,
#   pause.
# Modifiers: Super, Alt, Ctrl, Shift. At least one is required, except for
#   ScrollLock, Pause and F13-F24, which can be bound alone ("ScrollLock").
#   A bare binding fires only when no modifier is held. The modifier set must
#   match exactly, so "Ctrl+Alt+Ins" does not fire while Shift is also held.
#   Keys are not grabbed: the press still reaches the system, so ScrollLock
#   still toggles its LED.
# Two bindings sharing a combo both fire on one press, so whisrs warns at
#   startup when it finds a duplicate across [hotkeys], [[llm_commands]] and
#   [[agents]].
[hotkeys]
toggle = "Super+Shift+W"
cancel = "Super+Shift+D"
command = "Super+Shift+G"
speak = "Super+Shift+R"
recording = "Super+Shift+M"   # start or stop a recording (mic + system audio)

# Dictation-lifecycle hooks: pause playing MPRIS media while dictating, run
# shell commands fire-and-forget on dictation start/stop.  The child inherits the
# daemon's environment and stdout/stderr (journal under systemd --user).
# A recording (whisrs recording) fires none of them.
[hooks]
media_auto_pause = true        # pause MPRIS players that are playing, resume those on stop
# on_record_start = ""         # shell command on dictation start
# on_record_stop = ""          # shell command on dictation stop
```

`media_auto_pause` only touches players that report `PlaybackStatus = "Playing"`
when a dictation starts, and resumes exactly those when it stops. A tab you paused
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
  The text goes to the history, so `whisrs log` prints it and you can copy it
  from there. With `[general] history = false` there is no copy, so the result
  is discarded. Ask for a one-liner to get an injected result.
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

The refusal does not apply with `[input] clipboard_only = true`. whisrs then
types and pastes nothing, so a multi-line reply is copied to the clipboard like
any other. If you paste it into a shell yourself, the bracketed paste caveat
above still applies.

Terminal detection needs the compositor to report the focused window class,
which today means Hyprland, Niri, Sway and X11. On KDE and GNOME a terminal is
treated as an ordinary target, so a multi-line reply is typed there. Add any
class the built-in list misses to `[input] terminal_classes`.

Dictation follows the same rule with a gentler outcome. If a transcript or an
`llm_post_process` rewrite contains a line break and the focused window is a
terminal, each run of line breaks becomes a single space, whether the text is
typed or pasted. A `clipboard_only` copy keeps its line breaks. This uses the
same terminal detection, so it does not fire on KDE and GNOME. Control characters such as Escape and Backspace are removed from
everything whisrs types, pastes or copies, wherever it is going.

## Agent mode

Agent mode lets you talk to a coding agent. Press an agent's hotkey (or run
`whisrs agent <name>`), say what you want, and press again. whisrs sends the
words to a Claude Code, OpenCode, Codex or GitHub Copilot CLI session working in that
agent's `dir`, and
when the turn ends it speaks a short reply. The next press continues the same
session, so you can answer the question the agent asked or make the decision
it needs.

```toml
[tts]
enabled = true
backend = "groq"

[[agents]]
name = "jarvis"
hotkey = "Super+Shift+J"
new_session_hotkey = "Super+Shift+N"   # optional: a voice turn in a fresh session
dir = "~/Projects/whisrs"

[[agents]]
name = "coder"
harness = "opencode"
hotkey = "Super+Shift+K"
dir = "~/Projects/site"

[[agents]]
name = "cx"
harness = "codex"
hotkey = "Super+Shift+C"
dir = "~/Projects/api"

[[agents]]
name = "pilot"
harness = "copilot"
hotkey = "Super+Shift+P"
dir = "~/Projects/docs"
```

```
whisrs agent                       # list the agents: name, harness, idle | working | needs-you, and
                                   #   the id of each session: Claude Code's background id (what
                                   #   claude attach <id> takes), OpenCode's session id (what
                                   #   opencode --session <id> takes), Codex's thread id (what
                                   #   codex resume <id> takes), Copilot's session id (what
                                   #   copilot --resume=<id> takes)
whisrs agent jarvis                # record a voice turn (same as the hotkey; press again to stop)
whisrs agent jarvis --text "..."   # send a typed turn (`--text -` reads stdin)
whisrs agent jarvis --new          # record a voice turn that starts a fresh session (same as
                                   #   new_session_hotkey); with --text, send that text to one
whisrs agent jarvis --reset        # forget the session and send nothing; the next turn starts a new one
whisrs agent jarvis --stop         # stop the running turn (or the session's own work) and keep the session
```

What you need:

- Claude Code (`claude`), logged in. Tested on 2.1.301; it needs a version
  whose `claude --bg --resume` continues a session in place (2.1.257 or later).
- The agent's `dir` trusted in Claude Code: background sessions refuse a
  directory you never trusted. Open it with `claude` once and accept the trust
  prompt. whisrs tells you so when it hits this.
- Or, for `harness = "opencode"`: OpenCode 2 (`opencode`; tested on 2.0.29,
  and OpenCode 1 has no `run --standalone`), with a model it can use from
  `opencode run` (a provider you set up with `opencode auth`; the free
  OpenCode Zen models refused `opencode run` when tested). See "OpenCode"
  below.
- Or, for `harness = "codex"`: the Codex CLI (`codex`), signed in with
  `codex login` (a ChatGPT plan or an API key). Tested on 0.164.0 against a
  local mock model, not a real account. See "Codex" below.
- Or, for `harness = "copilot"`: the GitHub Copilot CLI (`copilot`), signed in
  with `copilot login`, or with a model provider of your own (Copilot's BYOK
  mode). With a GitHub account, every turn uses your Copilot plan's usage
  (AI credits, or premium requests on the legacy billing platform), as
  Copilot bills them. Tested on 1.0.103 against a local mock model only,
  never with a GitHub account. See "GitHub Copilot" below.
- `[tts] enabled = true` to hear the replies. Without it they arrive as
  notifications only, and `whisrsd` warns at startup.
- `whisrs restart` after adding or changing an `[[agents]]` entry: the daemon
  reads them only when it starts.

How a turn goes:

- The voice instruction is recorded like command mode's: the cues, the
  silence auto-stop, and every transcription backend. It shows on the agent
  overlay (below), not on the dictation overlay. Dictation is back to normal
  as soon as the transcript is ready, while the agent works.
- The reply is spoken when whisrs is idle. A reply that arrives while you
  dictate waits for the dictation to finish, and one that arrives during a
  `whisrs recording` waits until the recording stops (it would play into the
  call and land on the recording). A reply still playing when a recording
  starts is stopped and plays again once the recording stops. Several replies
  queue up.
- A voice instruction with nothing in it (too short, silent, or nothing
  recognized) is not sent, and whisrs says "I didn't catch that." A failed
  transcription is spoken too, in a short sentence, so both are heard with
  notifications off.
- Pressing the agent's hotkey while a reply plays stops it and starts
  recording, so answering the agent is one press. A dictation toggle,
  `whisrs command` or an `[[llm_commands]]` hotkey also stops an agent reply
  and goes ahead (a selection read-aloud still has to finish or be cancelled
  first). `whisrs cancel` while a reply plays also drops the replies queued
  after it.
- A press while that agent's turn is still running is refused with a short
  "Still working on the last one.", spoken once however often you press. When
  that agent's own reply or notice plays at that moment (that notice, or a
  reply the session produced on its own), the press stops it and is refused
  with no new spoken notice (with `[general] notify` on, a notification says
  why), so a press always silences its agent. Another agent's reply is left
  playing, and the notice is spoken after it. Other agents can run at the
  same time; only one recording exists at a time.
- A fresh session: `whisrs agent <name> --new`, or the agent's
  `new_session_hotkey`, records a voice turn like the normal hotkey, but the
  transcript starts a new session instead of continuing the old one. Press
  either key again (or run `--new` again) to stop and send; the press that
  started the recording decides. The old session is forgotten only once there
  is something to send, and stays in Claude Code, OpenCode, Codex or Copilot. `--new --text "..."` does
  the same with typed text, and `--reset` only forgets the session. Both are
  refused while a turn of that agent runs: stop it first with `--stop`. While
  that turn waits for you, the refusal says what it waits for ("Waiting for
  your approval to use Bash."), since a plain press answers it.
- With Claude Code, the session can also work on its own: when a command it
  started in the background finishes, Claude Code goes on without you. Its reply is spoken
  like any other, and while it works `whisrs agent` shows it as working and a
  new turn is refused rather than stopping that work: whisrs says "It's still
  running a background task." Claude Code counts a session as working for as
  long as one of its background commands runs, so a long-running one (a dev
  server) keeps refusing turns until it ends. To go on, run `whisrs agent
  <name> --stop`, which stops the session and ends that task with it, then
  send the instruction again. A refused instruction is never sent; with
  `[general] notify` on, the notification repeats it so you can send it again.
- When a turn fails (the session ended, `claude` was not found, the folder is
  not trusted), whisrs says so in a short sentence and, with `[general] notify`
  on, shows the full message as a notification.
- When a Claude Code agent waits for a permission prompt, `whisrs agent`
  shows it as needs-you and whisrs says so ("Waiting for your approval to use Bash."), for
  a turn you sent and for work the session does on its own alike. The next
  turn you send replaces the waiting one. To approve the prompt itself, open
  the session in Claude Code (`claude attach <id>`).

The spoken reply. whisrs asks the agent to end every final message with a short
reply inside `<spoken></spoken>` tags, written to be heard (`reply_instruction`
replaces that instruction). It does not rely on the model: it takes the last
`<spoken>` block, or the first two sentences when there is none, drops code,
links, file paths and markdown, and caps it at 400 characters. Anything outside
the tags stays in the session for when you look at it. When nothing speakable
is left, it says "Done."

Feedback levels. `feedback` sets what an agent says while it works, before its
reply:

| `feedback` | What you hear |
|---|---|
| `summary` | the reply only |
| `brief` (default) | a short acknowledgement when the agent starts using tools ("On it.", "Let me check."), then the reply |
| `conversational` | the acknowledgement, a one-sentence update at a real milestone (it found the cause, finished a big step), at least 20 seconds apart and never the same one twice, then the reply |

The agent writes these lines itself: whisrs adds a request for them to its
voice instruction, so acknowledgements and updates depend on the model
following that request. whisrs speaks only what the level allows, whatever the
model writes, and only a message the agent followed with a tool call, so the
final reply is never taken for one. They wait under the same rules as a reply
(not while you dictate or record), are dropped when they waited too long (8
seconds for an acknowledgement, 12 for an update) or when the reply arrives,
and are never shown as notifications. `whisrs cancel`, `--stop` and `--reset`
drop the ones still queued. A change to `feedback` needs `whisrs restart`
first, like any `[[agents]]` change. A lower level then applies from the
restart, since whisrs filters what it speaks. A higher one also needs a fresh
session (`whisrs agent <name> --new`): like `reply_instruction`, the request
is fixed when a session starts (with Copilot, both apply from the next turn:
whisrs gives Copilot the instruction on every turn). An unknown value gets a
warning at startup and means `brief`.

The agent overlay. With an `[[agents]]` entry, whisrs shows a row for each
agent at work in a screen corner, bottom right by default, apart from the
dictation overlay. Agent turns no longer show on the dictation overlay or the
tray: a voice turn's recording and transcription and the agent's speech show
here instead. A row is a shape (`style = "orb"`, rings and arcs, or `"face"`,
a small face) beside a card with the agent's name, its state, what it does and
for how long.

- States: listening (its voice turn records), transcribing, working,
  speaking (its reply, acknowledgement or notice plays), needs you (it waits
  for a permission prompt), done and error. Each state has its own color,
  except speaking and done, which share one.
- The activity line names what the agent does: "Running a command", "Reading
  files", "Editing files", "Searching the code", "Reading a web page",
  "Searching the web", "Working with a helper", "Planning". Tools from an MCP
  server (Model Context Protocol, how Claude Code reaches outside tools) go by
  the server: "Using the browser", "Reading your inbox", "Checking your
  calendar", "Reading your recordings", else "Using" and the server's name.
  Anything else reads "Working". While it waits: "Waiting for your approval".
- The timer counts the turn's time while it works, waits or speaks during the
  turn; listening and transcribing count from your press. A finished row says
  "Finished in 1:24" or "Failed after 0:12" instead.
- Up to four rows, stacked from the corner in config order. With more agents
  at work, three rows show and a fourth says "+N more".
- A done or error row stays while its reply waits or plays, then fades out
  about three seconds after it played.
- Work a session does on its own shows too: a turn refused with "It's still
  running a background task." shows that work as working, and a permission
  prompt it hits as needs you, until the work ends: the session's `Stop`
  hook arrives (the event Claude Code sends when the session stops working;
  see "How whisrs hears the end of a turn" below), which ends the row as done
  when it carries a reply, or `claude agents` lists the session as neither
  working nor waiting. whisrs runs that listing ten seconds after such a row
  appears, then waits twice as long before each next one, up to two minutes;
  a new row or a turn you start brings the wait back to ten seconds.
- `enabled` follows `[general] overlay` unless you set it. An unknown `style`
  or `corner` (an edge center is not a corner) gets a warning at startup and
  the default.
- It draws on Wayland compositors with layer-shell (the protocol that lets a
  program place panels and overlays, as on Hyprland and Sway) and on X11. On
  X11 it scales by `Xft.dpi` (the X resource that sets the font DPI, set with
  `xrdb`; 96 is 1x, up to 3x). On Wayland it follows the output's whole scale;
  a fractional scale is not supported yet, so it draws at the next whole one
  and the compositor scales it down.
- GNOME on Wayland has no agent overlay yet (GNOME has no layer-shell, and the
  GNOME extension does not draw it yet). There, and whenever the overlay is
  not on screen, the "Speak to 'jarvis'..." and "Sending to 'jarvis'..."
  notifications stand in for it (with `[general] notify` on).

With Claude Code, `permissions` maps to its permission modes (OpenCode's,
Codex's and Copilot's mappings are under "OpenCode", "Codex" and "GitHub
Copilot" below):

| `permissions` | Claude Code |
|---|---|
| `auto` (default) | `--permission-mode auto` |
| `bypass` | `--permission-mode bypassPermissions` |
| `ask` | `--permission-mode default`: asks before acting, even when your own default mode is `auto` |

With Claude Code, `permissions` and `args` are passed when a session starts. A later turn resumes
the session with the options it was started with (its hooks, the permission
mode and your `args` included). So a change to them needs `whisrs restart`
first, like any `[[agents]]` change, and then applies from the next new
session (`whisrs agent <name> --new`). The same goes for `reply_instruction`:
the session keeps the instruction it started with.

`claude --bg` takes the prompt only as an argument, so with Claude Code your
words (and on a session's first turn the voice instruction) are on the command
line of the `claude --bg` that whisrs runs, which other processes can read
(`ps` shows it) until it returns, once the session has started. OpenCode,
Codex and Copilot get your words on stdin.

One option you may want in `args` is `--remote-control` (for example
`args = ["--remote-control", "jarvis"]`; the name after it is optional).
Remote Control is a Claude Code option that lets you follow and answer the
session from the Claude app. whisrs passes it only when you list it in `args`.

How a later turn continues the session: whisrs stops the session's process
(`claude stop`), waits until that process has exited, then resumes the session
with no other flag. Claude Code needs both: a resume while the old process
still runs, or one with any flag, starts a copy of the session instead, which
comes back without the hooks, the permission mode and your `args` (Remote
Control among them). whisrs remembers the process every `claude agents`
listing shows, so a session stopped moments before (by `--stop`, say), which
Claude Code already lists without a process, is waited for the same way.
The wait is capped at ten seconds; a process that is still running then fails
the turn ("Claude Code did not stop the session's last run in time, so this
turn was not sent."), and nothing is sent. whisrs never adopts a copy: if a resume starts one
anyway, whisrs stops that copy, waits for it to exit and removes it (never
the session itself), waits for the original again and resumes once more. If
that also starts a copy, the turn fails with
"Claude Code could not continue the session" and `whisrs agent <name> --new`
starts a fresh one.

Sessions. Each Claude Code agent's session is a background session named
`whisrs-<name>`: it shows up in `claude agents`, and `claude attach <id>` opens it
(`<id>` is the background id `whisrs agent` shows, which `claude logs` and
`claude stop` take too). whisrs keeps its session ids in
`~/.local/share/whisrs/agents.json` and only ever stops, resumes or reads
sessions it started, by those ids; your other sessions are never touched.
Sessions outlive the daemon: a turn that is still running when `whisrsd`
restarts is watched again, so its reply is still spoken. A restart that lands
while a new session is being started, before whisrs learned its id, leaves a
session whisrs cannot find without matching it by name, which it never does:
it says it lost track of that turn, which may still be running (`claude agents`
lists it). A session you delete
(`claude rm`) is forgotten, and the next turn starts a new one. Changing an
agent's `harness` or `dir` starts a new session too, from the restart that
applies it.

How whisrs hears the end of a turn: it starts sessions with `--settings`
holding a `Stop` hook and a permission-prompt `Notification` hook, both running
`whisrs agent-hook`, which forwards the event to the daemon and always exits
within two seconds. Your own Claude Code settings are not changed. While a turn
runs, whisrs also checks `claude agents` every ten seconds, which covers a
missed hook.

`claude` runs in the agent's `dir` with the daemon's environment, minus the
variables a parent Claude Code session sets for the processes it runs
(`CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_SESSION_ID`,
`CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ATTENDED`,
`CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`,
`CLAUDE_CODE_SSE_PORT`, `CLAUDE_CODE_EXECPATH`, `CLAUDE_PID`, `CLAUDE_EFFORT`,
`CLAUDE_PROJECT_DIR`, `CLAUDE_JOB_DIR`, `AI_AGENT`, so a daemon started from
inside a session works) and whisrs's own `WHISRS_*` keys. Everything else is
passed on, Claude Code's own settings included (`CLAUDE_CONFIG_DIR`,
`CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_USE_BEDROCK`, ...). If no Claude Code
background service is running yet, the first `claude --bg` starts one, and it
keeps that environment for every background session it hosts until it exits.
That service hosts your own background sessions too, so when `whisrsd` runs as
a systemd service, whisrs starts `claude --bg` in a scope of its own
(`systemd-run --user --scope`): restarting or stopping `whisrs.service` then
leaves the service and every session running. Without `systemd-run` it starts
`claude` directly.

### OpenCode

With `harness = "opencode"`, each turn is one `opencode run` process, which
ends with the turn:

```
opencode run --standalone --format json --session <id> [--title whisrs-<name> <args>] [--auto]
```

The words go in on stdin. `--standalone` gives the turn a private OpenCode
server that ends with it, instead of OpenCode's shared background service,
which would outlive the turn, ignore the environment whisrs passes, and keep a
turn running after whisrs stopped it. A server starts with every turn, which
takes a second or two.

Sessions. whisrs makes up the session id (`ses_...`, in OpenCode's own form)
and passes it on every turn: the first turn creates the session, titled
`whisrs-<name>`, and every later one continues it. `whisrs agent` shows the
whole id; `opencode --session <id>` opens the session in OpenCode, and it is
in `opencode session list`. The id is the agent's as soon as the turn's
`opencode run` starts, so a first turn you stop (or a restart cuts off) before
OpenCode printed anything keeps it, and the next turn continues that session
instead of leaving it behind. Until a turn shows the agent at work in it (a
reply or a tool call), each turn starts the session over on the same id, with
the title, `args` and the voice instruction. So a first turn that only failed
(a mistyped model in `args`, say) does not tie the session to it: fix `args`,
run `whisrs restart`, and the next turn uses them. A turn that never started
`opencode` (it was not found, say) leaves no session behind.

The voice instruction (the built-in one or your `reply_instruction`, with the
request of your `feedback` level) opens the first message of a session.
`opencode run` has no option for a system prompt, and the ways around that
either replace OpenCode's own prompt or did not reach the model. In the
session's history, the instruction holds for every later turn. It shows in the
session's first message when you open it in OpenCode, and a compaction of a
long session may summarize it away (start a fresh one with `--new`).

`permissions` for OpenCode:

| `permissions` | OpenCode |
|---|---|
| `auto` (default) | no flag: your own OpenCode rules (OpenCode allows most tools unless you set rules) |
| `bypass` | `--auto`: approves every permission request; rules that deny still apply |
| `ask` | adds rules that ask before shell commands and file changes (`shell` and `edit`) to `OPENCODE_CONFIG_CONTENT`, after your own config rules (the last match wins): reading and searching work, commands and edits are refused unless OpenCode already has an approval saved for them (see below) |

Nobody can answer OpenCode's permission questions during a turn: a request
that would ask is refused, the agent is told so and goes on without it, and
the reply usually says what it could not do. So an OpenCode agent never shows
needs-you, and a turn never waits for you. With `ask`, a daemon whose
`OPENCODE_CONFIG_CONTENT` is not a plain JSON object fails the turn instead of
running it without the rules.

OpenCode's own approvals win over the rules `ask` adds: an "always" answer you
gave in OpenCode (saved for the project) and what a session allows come after
every config rule, and the last match wins. So `ask` refuses commands and
edits unless OpenCode already has an approval saved for them: a command you
approved with "always" in OpenCode still runs.

Which keys apply when: `permissions` is passed on every turn, so after
`whisrs restart` a change applies from the next turn. `args` is passed on the
first turn of a session: a model (`args = ["-m", "anthropic/claude-sonnet-4-5"]`)
or an agent (`--agent plan`) chosen there stays the session's. `reply_instruction`
and `feedback` are fixed when a session starts, as with Claude Code. `command`
is the `opencode` binary.

What you hear and see works as with Claude Code, with two differences.
OpenCode reports a tool call when it finishes, so the overlay's activity line,
and the acknowledgement the agent wrote before its first tool call, come when
that call ends. And between turns no OpenCode process runs: a session never
works on its own, and a new turn is never refused for that.

Each turn runs in a systemd scope of its own (`systemd-run --user --scope`, a
unit named `whisrs-agent-<instance>-<name>-<turn>.scope`; `<instance>` is
eight hex digits hashed from the path of whisrs's `agents.json`, so a second
daemon with a data directory of its own leaves these scopes alone). Through
the scope, a stop reaches every process the turn started, also one that put
itself in a session of its own.
`--stop` interrupts the turn: whisrs sends SIGINT, then SIGTERM and SIGKILL to
whatever of the turn is still running a few seconds later (`opencode run`, its
process group, every process of its tree and the scope), and returns once all
of it is gone. Without `systemd-run`, or a user systemd it can reach, the turn
starts directly, and a stop reaches the process group and every process of the
tree whisrs can still find (one whose parent already exited is out of its
reach). The session keeps what it did; the next turn continues it, the stopped
tool call in its history.

A daemon restart during a turn ends that turn: whisrs has the kernel interrupt
`opencode run` (SIGINT) when the daemon exits or is killed, and when the
daemon starts again it stops anything an earlier turn left in its scope, a
background job included (a process that ignored the SIGINT, or one a turn
that ended normally left running, a dev server started with `&` say), before
it runs any turn, as a restart of `whisrs.service` stopping its own processes
would. It does so even when you removed every `[[agents]]` entry since. The
scope is not part of `whisrs.service`, so such a process outlives a stopped
service until whisrsd starts again. The restarted daemon says "whisrs restarted during that turn,
which stopped it before OpenCode replied." The session keeps what it did so
far, and the next turn continues it. When the agent had not been seen at work
in the session yet, it says instead that the turn may not have reached
OpenCode at all, so say it again.

`opencode` runs in the agent's `dir`, with `PWD` set to it (OpenCode takes its
directory from `PWD`), and the daemon's environment minus what a parent Claude
Code session sets (the list above), what a parent OpenCode session sets
(`OPENCODE`, `OPENCODE_SESSION_ID`, `OPENCODE_TERMINAL`, `AGENT`,
`OPENCODE_PASSWORD`, `OPENCODE_SERVER_PASSWORD`) and whisrs's own `WHISRS_*`
keys. Everything else is passed on, OpenCode's own settings included
(`OPENCODE_CONFIG`, `OPENCODE_CONFIG_CONTENT`, `OPENCODE_CONFIG_DIR`, the XDG
directories).

### Codex

With `harness = "codex"`, each turn is one `codex exec` process, which ends
with the turn. This was tested on Codex CLI 0.164.0 against a local mock
model, not a real account. The first turn starts a session, every later one
resumes it:

```
codex exec --json <permissions> -c analytics.enabled=false -c developer_instructions=<instruction> [<args>] --skip-git-repo-check -
codex exec --json <permissions> -c analytics.enabled=false -c developer_instructions=<instruction> [<args>] --skip-git-repo-check resume <id> -
```

The words go in on stdin. `--skip-git-repo-check` lets `dir` be any folder,
not only a git repository. It comes after your `args` so that an option there
that takes several values (`-i`/`--image`) ends before `resume`, the id and
the `-` of the prompt; leave `--skip-git-repo-check` itself out of `args`
(Codex refuses it twice). `analytics.enabled=false` turns Codex's usage
metrics off for these runs: with them on, `codex exec` sends them after the
turn and exits only once they are sent, which held every reply back by 4 to
10 seconds when tested. To keep them, put `"-c", "analytics.enabled=true"` in
`args`: it comes later, so it wins.

Sessions. Codex makes up the session id (a thread id, a UUID), and whisrs
reads it from the first line Codex prints. `whisrs agent` shows the whole id;
`codex resume <id>` opens the session in Codex. whisrs resumes a session only
by that id, never by a name. A turn that failed after Codex started the
session (the model refused the request, say) keeps it, and the next turn
continues it. If Codex no longer has the session (you deleted or archived
it), the turn fails with "Codex no longer has this agent's session" and
`whisrs agent <name> --new` starts a new one.

The voice instruction (the built-in one or your `reply_instruction`, with the
request of your `feedback` level) goes in as Codex's developer instructions,
on every turn. The model gets it as a developer message, so it never shows as
your words. Passing it again on a later turn does not repeat it, and it
survives a compaction of a long session, which drops an instruction that was
given only on the first turn (both measured). A session keeps the
instruction it started with (a changed one passed on a later turn is not
added, measured), so after `whisrs restart` a changed instruction applies from
the next new session (`whisrs agent <name> --new`), as with Claude Code and
OpenCode. It replaces any
`developer_instructions` you set in Codex's own `config.toml` or a profile:
put text of your own in `reply_instruction`.

`permissions` for Codex:

| `permissions` | Codex |
|---|---|
| `auto` (default) | `--approve-for-me`: the workspace-write sandbox, so the agent changes files in `dir` (network off); a command that needs more goes to Codex's automatic reviewer, which approves or refuses it (one more model call) |
| `bypass` | `--dangerously-bypass-approvals-and-sandbox`: no sandbox and no approvals |
| `ask` | `--sandbox read-only`: the agent reads files and runs commands that only read; anything that would change something is refused, so it tells you what it would do instead |

Nobody can answer Codex's approval requests during a turn: `codex exec`
refuses what it may not do, the agent is told so and goes on, and the reply
usually says what it could not do. So a Codex agent never shows needs-you, and
a turn never waits for you. Leave sandbox and approval flags out of `args`:
`--approve-for-me` and `--sandbox` together make Codex refuse to start.

When `dir` is a git repository, a turn with `auto` or `bypass` makes Codex
record the repository as trusted in its own `config.toml`
(`[projects."<dir>"] trust_level = "trusted"`), as running Codex there in a
writable mode yourself does, and Codex then also loads that project's own
`.codex/` settings. `ask` records nothing (both measured).

Which keys apply when: `permissions` and `args` are passed on every turn,
since `codex exec resume` keeps neither from the first turn (a `-m` model
given only on the first turn was gone on the next, measured). So after
`whisrs restart` a change to either applies from the next turn.
`reply_instruction` and `feedback` are fixed when a session starts, as with
Claude Code. `command` is the `codex` binary.

Billing. `codex exec` uses the sign-in Codex already has: your ChatGPT plan,
or an API key at API prices. A `CODEX_API_KEY` in the daemon's environment
takes precedence over a ChatGPT sign-in, and whisrs passes it on. With
`auto`, each review by the automatic reviewer is one more model call. When
Codex is not signed in, the turn fails after about 40 seconds of retries with
"it is not signed in, or its sign-in expired": run `codex login` in a
terminal. When the daemon's environment has a `CODEX_API_KEY` (which Codex
uses before any sign-in), a refused key fails the turn with "the API key in
CODEX_API_KEY was refused" instead.

What you hear and see works as with Claude Code, with two differences. Codex
reports a tool call when it starts, so the overlay's activity line, and the
acknowledgement the agent wrote before its first tool call, come as soon as
the call begins (a command the read-only sandbox blocked showed no activity
when tested: Codex printed no line for it). And between turns no Codex process
runs: a session never works on its own, and a new turn is never refused for
that.

`--stop` interrupts the turn as with OpenCode (in the turn's own systemd
scope): whisrs sends `codex exec` SIGINT, then SIGTERM and SIGKILL to whatever
of the turn is still running a few seconds later, and returns once all of it
is gone. Codex ends the command it runs with it, sandboxed or not. The session
keeps what it did; the next turn continues it. Stopping a command that runs in
the workspace-write sandbox (`auto`) can leave empty `.git`, `.codex`,
`.agents` and `.aws` directories in `dir` where it had none: the sandbox's
mount points, which Codex does not remove when it is interrupted (measured).
Delete them if they are in the way.

A daemon restart during a turn ends that turn, as with OpenCode (the restarted
daemon stops anything an earlier turn left in its scope first, a background
job included), and the restarted daemon says "whisrs restarted during that
turn, which stopped it before Codex replied." The session keeps what it did so far, and the next turn continues
it.

`codex` runs in the agent's `dir`, with `PWD` set to it, and the daemon's
environment minus what a parent Claude Code session sets (the list above),
what a parent Codex session sets for the commands it runs (`CODEX_THREAD_ID`,
`CODEX_SESSION_ID`, `CODEX_TOOL_CALL_ID`, `CODEX_CI`, `CODEX_SANDBOX`,
`CODEX_SANDBOX_NETWORK_DISABLED`, `CODEX_VERSION`,
`CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS`), `RUST_LOG` (whisrsd's log level,
which Codex would take as its own and print its logs at) and whisrs's own
`WHISRS_*` keys. Everything else is passed on, Codex's own settings included
(`CODEX_HOME`, `CODEX_API_KEY`).

### GitHub Copilot

With `harness = "copilot"`, each turn is one `copilot` process (the GitHub
Copilot CLI in its prompt mode), which ends with the turn. This was tested on
Copilot CLI 1.0.103 against a local mock model only (Copilot's offline mode
with a model provider of its own, `COPILOT_OFFLINE=true` and
`COPILOT_PROVIDER_BASE_URL`), never with a GitHub account. The first turn
starts a session, every later one resumes it:

```
copilot --output-format json --session-id=<id> <permissions> [<args>]
copilot --output-format json --resume=<id> <permissions> [<args>]
```

The words go in on stdin, which Copilot's prompt mode reads when there is no
`-p`, so they never show in the process list. `command` must be the `copilot`
binary itself: `gh copilot` runs the same CLI, but cannot take these
arguments as they are.

A turn that starts with `/` runs a Copilot command: prompt mode reads
`/name ...` as a slash command or a skill, as if you typed it into Copilot,
not as words for the agent (a leading space would not stop `/compact`).
whisrs passes your words as they are.

Sessions. whisrs makes up the session id (a UUID) and starts the session with
it. `whisrs agent` shows the whole id; `copilot --resume=<id>` opens the
session in Copilot, which names it after its first words, as it does any
session. Until a turn shows the agent at work in the session (a reply or a
tool call), each turn passes `--session-id` again: it starts the session when
Copilot never made it (a first turn you stopped at once, say) and continues
it when it did. After that, whisrs resumes the session only by its id, never
by a name. If Copilot no longer has it, the turn fails with "Copilot no longer
has this agent's session" and `whisrs agent <name> --new` starts a new one.

The voice instruction (the built-in one or your `reply_instruction`, with the
request of your `feedback` level) goes to Copilot as a custom instructions
file of whisrs's own,
`~/.local/share/whisrs/copilot/<name>/.github/instructions/whisrs-voice.instructions.md`,
whose directory whisrs adds to `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` on every
turn, after any directories the daemon's environment already lists there.
Copilot puts it into the system prompt of every request of the turn, so it
never shows as your words, and nothing of it is kept in the session's history:
opened in Copilot yourself, the session runs without it. whisrs writes the
file before each turn, so after `whisrs restart` a changed `reply_instruction`
or `feedback` applies from the next turn. `--no-custom-instructions` in `args`
turns it off, with your own instruction files.

`permissions` for Copilot:

| `permissions` | Copilot |
|---|---|
| `auto` (default) | `--allow-all-tools`: commands and file changes in `dir` run without asking; a file outside `dir` and the temp directory, or a URL, still needs an approval, so it is refused |
| `bypass` | `--yolo`: every tool, path and URL allowed |
| `ask` | `--deny-tool=shell --deny-tool=write`: the agent reads and searches files; commands and file changes are refused, also when your Copilot settings or `args` allow them (a deny rule always wins, measured). No rule covers a tool from an MCP server (see below) |

Nobody can answer Copilot's approval requests during a turn: prompt mode
refuses what needs one, the agent is told so and goes on, and the reply
usually says what it could not do. So a Copilot agent never shows needs-you,
and a turn never waits for you. A tool from an MCP server keeps Copilot's own
rules: one that needs an approval is refused. With `ask`, that is all that
stops one: its deny rules do not cover MCP tools, so an approval you saved in
Copilot itself ("Yes, and don't ask again for tool ... in this repo", or "in
this directory") may let one run, as it would in Copilot. `COPILOT_ALLOW_ALL` in
the daemon's environment is not passed on, so `permissions` decides.

Which keys apply when: `permissions` and `args` are passed on every turn,
since Copilot keeps neither between turns (an `--agent` given only on the
first turn was gone on the next; a `--model` stays the session's either way;
both measured). So after `whisrs restart` a change to either applies from the
next turn. Leave `-p`, `--output-format`, `--session-id`, `--resume`, `--name`
and `--trust-folder` out of `args`: whisrs passes the first ones itself, and
Copilot refuses `--trust-folder` beside `--session-id` and `--resume`, and
`--name` beside a session it already has (measured).

Billing. With a GitHub account, every turn uses your Copilot plan's usage (AI
credits, or premium requests on the legacy billing platform), counted by
Copilot's own billing as the same prompt typed into Copilot would be
(`copilot help billing` says where to see it). Copilot uses the sign-in it
already has: `copilot login`, or a token in `COPILOT_GITHUB_TOKEN`, `GH_TOKEN`
or `GITHUB_TOKEN`, which whisrs passes on (a `GITHUB_TOKEN` meant for
something else then wins over your sign-in). With a model provider of your own
(`COPILOT_PROVIDER_BASE_URL` and the other `COPILOT_PROVIDER_*` variables in
the daemon's environment), no GitHub sign-in is used. When Copilot is not
signed in, the turn fails with "Copilot is not signed in to GitHub": run
`copilot login` in a terminal. A provider that refuses its key, or that does
not answer, fails the turn with Copilot's own message ("Authentication failed
with provider at ...", "Could not connect to local model provider at ...").
An error Copilot itself does not count as the turn's failure does not fail it
here either: a hook's, a helper agent's, or a failed model call after which
the agent still replies.

What you hear and see works as with Claude Code, with two differences.
Copilot reports a tool call when it starts, so the overlay's activity line,
and the acknowledgement the agent wrote before its first tool call, come as
soon as the call begins. And between turns no Copilot process runs: a session
never works on its own, and a new turn is never refused for that.

`--stop` interrupts the turn as with OpenCode (in the turn's own systemd
scope): whisrs sends `copilot` SIGINT, then SIGTERM and SIGKILL to whatever of
the turn is still running a few seconds later, and returns once all of it is
gone. Copilot ends the command it runs with it. That command runs in a session
of its own and would outlive a SIGKILL of Copilot's process group; the scope
reaches it (measured: no process left after a `--stop` during a command). The
session keeps what it did; the next turn continues it. A turn something else
interrupts fails with "it was interrupted before it replied" (Copilot itself
reports such a turn as a success).

A daemon restart during a turn ends that turn, as with OpenCode (the restarted
daemon stops anything an earlier turn left in its scope first, a background
job included), and the restarted daemon says "whisrs restarted during that
turn, which stopped it before Copilot replied." The session keeps what it did so far, and the next
turn continues it.

`copilot` runs in the agent's `dir`, with `PWD` set to it, and the daemon's
environment minus what a parent Claude Code session sets (the list above),
what a parent Copilot session sets for the commands it runs (`COPILOT_CLI`,
`COPILOT_AGENT_SESSION_ID`, `COPILOT_CLI_BINARY_VERSION`,
`COPILOT_CLI_RESOLVED_DIST_DIR`, `COPILOT_LOADER_PID`), `COPILOT_ALLOW_ALL`,
`COPILOT_PROMPT_TURN_DELIMITER` (which would split your words into several
prompts) and whisrs's own `WHISRS_*` keys. Everything else is passed on, Copilot's own
settings included (`COPILOT_HOME`, the tokens, `COPILOT_OFFLINE`,
`COPILOT_MODEL`, the `COPILOT_PROVIDER_*` variables). The `GIT_*` variables a
parent Copilot session sets pass too: Copilot sets them again for its own
commands. Copilot updates itself by default, so what it prints can change
without a whisrs release: whisrs reads the output of 1.0.103.

## Choosing the microphone

`[audio] device` picks the input whisrs records from:

- `"default"` (or empty): the system default input, through ALSA. With the PipeWire or PulseAudio ALSA plugin installed (the usual setup) this follows the default source set in your sound settings.
- A PulseAudio or PipeWire source name, such as `alsa_input.usb-Blue_Microphones_Yeti-00.analog-stereo`. List them with `pactl list short sources` (second column), or pick one from `whisrs config` under the audio device item. The source description shown in your sound settings also works. Monitor sources (names ending in `.monitor`) record what your speakers play.
- An ALSA PCM name, such as `pipewire`, `plughw:1,0` or `sysdefault:CARD=PCH` (see `arecord -L`). This also covers systems without a PulseAudio or PipeWire server.

PulseAudio/PipeWire sources are checked first, then ALSA names. A name that matches nothing logs a warning listing the valid names and falls back to the system default. The daemon reads the device when it starts, so restart it after a change (`whisrs restart`); its startup log shows which device the name resolved to. Audio cues and read-aloud playback always use the default output device.

### Speech detection

`[audio] vad` decides what counts as speech: when auto-stop fires, whether a batch dictation, command or agent turn is worth sending, and where a recording track holds speech.

- `"earshot"` (the default): a small learned voice activity detector that scores every 16 ms of audio. A steady fan or hum does not count as speech, and a quiet voice still does. During dictation, command mode, `[[llm_commands]]` and agent turns, a voice far under yours (a TV, a colleague behind you) does not count either; a recording counts every voice.
- `"rms"`: the loudness test of whisrs 0.1.31 (a block is speech when it is louder than about -50 dBFS). Use it if the model handles your mic badly. In a noisy room it may never auto-stop.

An unknown value warns at startup and uses `"earshot"`. Restart the daemon after a change.

Audio that is not sent is skipped with a notification that names the reason and the level whisrs measured (levels are in dB relative to full scale; a normal speaking voice sits near -40):

- "Skipped: the audio was silent (peak -82 dB). Is the mic muted?"
- "Skipped: the audio was too quiet (speech at -68 dB)"
- "Skipped: the audio was without speech (background at -48 dB)"

Command mode and agent turns say the same in their own words ("Agent 'jarvis': the audio was too quiet (speech at -68 dB), nothing was sent"). A streaming dictation that typed nothing says "Nothing typed: the audio was ..." with the same reason. Each dictation, command and agent turn also logs one line of levels (speech, background, SNR, peak, clipping) to the daemon log. [Noisy room or quiet mic](troubleshooting.md#noisy-room-or-quiet-mic) explains what to do about each.

## Recording

The recording feature records two tracks at once: you, through your
microphone, and what your computer plays, through the system audio. When you
stop, both tracks are transcribed and merged into one timed transcript, saved
as Markdown next to a JSON file that holds the same lines with their
timestamps.

Uses that work today:

- meetings and calls
- lectures, videos and podcasts
- language practice against a video
- interviews

`whisrs recording start [--title ...]` (or the `[hotkeys] recording` binding,
or "Start recording (mic + system audio)" in the tray menu) starts a
recording; `whisrs recording stop` ends it and queues the transcript.
`whisrs recording cancel` stops and deletes it. The `recording` hotkey and
`whisrs recording toggle` (for a compositor binding) start a recording when
none is in progress and stop the one that is.

**Two tracks.** Your microphone and the system audio (what your speakers or
headphones play) are recorded as separate files and transcribed separately.
Your track is labeled with `user_name`, and everything from the system track is
labeled "Others" (`--others NAME` on `start` or `toggle` names it otherwise),
unless its voices are told apart (see "Speakers" below).

**Speakers.** whisrs can tell apart the voices on the system track
(`diarization`). Each voice becomes a speaker, numbered in the order they
first speak: the transcript shows them as "Speaker 1", "Speaker 2" and so on,
and `recording.json` lists them under `speakers` with their ids (`S1`, `S2`,
...), how long each one talks (`talk_ms`) and the method used
(`diarization`, `off` when no voice was found). Nothing is guessed: when the
system audio holds a single voice, it keeps the others label ("Others", or
the name given with `--others`). A voice with less than 5 seconds of talk
and under 2 % of its track is marked `minor` in `recording.json`, and its
lines stay in the transcript; it does not count as a second voice, so a
single main voice keeps the others label beside minor ones shown as
"Speaker N". Speech the voices could not be matched to stays under the
others label. The transcript's front matter lists the speakers that have a
line in it; `recording.json` lists every speaker, so `whisrs recording
name` still finds one that said nothing.

`diarization = "backend"` takes the speakers the transcription backend labels:
Deepgram (`diarize_model=latest`, sent only with the recording's own
requests, never with dictation) or an `asr-sidecar` that returns
`segments[].speaker`. `diarization = "sidecar"` asks a diarization sidecar at
`diarization_url` (see below) while any backend transcribes the words.
`"auto"` (the default) uses the sidecar when `diarization_url` is set, else
Deepgram's speakers with Deepgram, and keeps one speaker per track with every
other backend. `"off"` always keeps one speaker per track. Deepgram includes speaker separation in its
pre-recorded price, but not with its Whisper models: with one of those as
`[deepgram] model`, each track stays one speaker. A backend labels voices
per request, so a track sent as several chunks has its voices told apart per
chunk, and the same person in two chunks becomes two speakers. If Deepgram
refuses speaker separation (an HTTP 400 about diarization, for a project
that cannot have it), the track is sent again without it: the transcript is
written with one speaker for that track, and its notes say why. With Deepgram
a chunk holds an hour of speech: over an hour of speech on one track,
speakers are separated per hour. For `asr-sidecar` a chunk is 10 minutes.

In an in-person meeting where several people share your mic, set
`shared_mic = true`, or pass `--shared-mic` to `start`, `toggle` or `retry`
(`--shared-mic=false` turns it off for one recording). The mic's voices are
then told apart too (`M1`, `M2`, ...), and the mic is you only when it holds
a single voice. With several, mic speech no voice was matched to is shown
as "Mic". A recording keeps the `shared_mic` it started with; a retry
given the option replaces it.

**Naming the speakers.** A voice gets a name only when whisrs can tell who
it is; every other voice stays "Speaker N". These steps run in order, and
the first one that names a speaker wins:

1. A name you give with `whisrs recording name` (see below) wins over
   everything else.
2. Your mic is you (`user_name`), when it holds one voice.
3. `--with "Sarah, Omar"` on `start` or `toggle` says who the recording is
   with: names separated by commas, at most 20, each up to 64 characters.
   When the system audio holds exactly one voice and you list exactly one
   person, that voice is them.
4. With `name_speakers = true`, the LLM of the `[llm]` section (the one
   command mode uses) is asked who the voices still unnamed are. It must
   quote the words that show each name, such as "I'm Sarah" or "Thanks,
   Sarah", and a name is kept only when that quote's words are words of
   one line of the transcript, in order (case, spacing and punctuation
   aside), and hold the name or its first name. When you listed people,
   the name must be one of them: a full name, a first name, or a short
   form of three letters or more that starts a first name ("Sam" for
   "Samantha"), and the name is written as you listed it. When you did
   not, only the part of the name the quote holds is kept ("Thanks, Sarah"
   names "Sarah", never "Sarah Jones"). A name
   the LLM gives two speakers, a name another speaker already has, and
   your own name are refused. Anything else stays unnamed.
5. When every voice on the system audio but one has a name, one person you
   listed is left, there are as many voices as people listed, and
   `shared_mic` is off, the last voice is that person.
6. Everyone else is "Speaker N", numbered in the order they first speak. A
   single voice on the system audio keeps the others label instead.

`name_speakers` is off by default, because it sends parts of every
transcript to the `[llm]` endpoint: the title, your name, the people you
listed, and for each voice it asks about, its first 2000 or so characters of
speech with the lines around them. Point `[llm] api_url` at a local server
to keep them on your machine. The LLM is only asked when a voice is left to
name after the steps before it, gets 60 seconds to answer, and is told that
the transcript is data, not instructions, and that the speaker labels can be
wrong. If it fails (no `[llm]` section, no answer in time, an answer that is
not JSON), the transcript is still written, without its names, and
`recording.json` notes why under `errors`.

A `minor` voice is never named after someone you listed, and does not count
as a voice. Your own name in the `--with` list (your full name, your first
name, or a short form of it) is you and is not counted.
With a shared mic that holds other voices, one person listed is not enough
to name the one voice on the system audio, since that person may be in the
room with you. `--with` names only voices that were told apart: with one
speaker per track, name the system audio with `--others` instead.
`whisrs recording retry <id> --with "..."` replaces the names, and
`--with ""` clears them. `recording.json` keeps them as `participants`, and
each speaker's `source` says how it got its name: `user`, `self`, `with`,
`llm` (with the quote as `evidence`), `last`, `channel` (the others label)
or `none` (Speaker N).

To name a speaker yourself, give the recording, the speaker as its id
(`S2`), as "Speaker 2" or by the name it has now, and the name:

```
whisrs recording name last "Speaker 2" Sarah
whisrs recording name last S3 Omar Haddad
whisrs recording name last Sarah ""        # clear the name you gave
```

The transcript and `recording.json` are rewritten at once, without
transcribing anything again, and the other steps run again around your
name: the voices still unnamed are numbered again (so "Speaker 3" can
become "Speaker 2"), and the last voice left can now be the last person you
listed. The command prints each speaker's id, name and source. Your names
are kept in `recording.json` as `speaker_names`, apart from the computed
`speakers`, so `whisrs recording retry` keeps them. A retry sends the audio
again, and the voices can come back numbered differently, so a name you
gave a voice remembers where that voice first spoke (`anchor`: its track
and `first_ms`) and follows it: after a retry it goes to the voice first
heard on that track within a second of that moment, whatever its number is
now, nearest first, each name and each voice used once. When the track
has voices but none of them is, the name is dropped rather than put on
someone else: the transcript's notes ask you to name the speakers again,
and the notification that the transcript is ready says how many names
were dropped. When a retry tells no voices apart on that track (the
diarization sidecar was down, Deepgram refused), the name is kept as it is
and comes back with the next retry that does. A name
you gave a track's own speaker (you, or the system audio when its voices
are not told apart) stays with it. Clearing a name you gave over the LLM's
brings the LLM's name back only with the next retry, which asks it again.
A recording that is queued or being transcribed is refused until its
transcript is ready.

**A diarization sidecar.** A local server that only finds who speaks when,
so speakers can be told apart offline or with a backend that cannot do it
(Groq, OpenAI, `local-whisper`). whisrs sends it each separated track once,
as the speech only (the silences cut out, as for transcription), and lines up
the transcript's words with the turns it returns: each word takes the turn it
overlaps most, else the nearest one within a second. A line whose words fall
in two turns is split between them; a line without word timings
(`local-whisper`) takes the turn it overlaps most. The contract:

```
POST {diarization_url}   multipart form: file = the track's speech as a 16 kHz mono WAV,
                         num_speakers = N (only when N > 0)
200 {"segments": [{"start": 1.20, "end": 4.85, "speaker": "SPEAKER_00"}]}
```

`start` and `end` are seconds into the WAV, and `speaker` is a string or an
integer that only has to be consistent within one request. `num_speakers`
(or `--num-speakers N` on `start`, `toggle` and `retry`, 0 to 20) tells the
sidecar how many voices the system audio holds, or your mic when the
recording has no system audio; 0, the default, lets the sidecar decide. A
recording keeps the number it started with, and a retry given the option
replaces it.

Two ready sidecars are in
[`contrib/diarization-sidecars/`](../contrib/diarization-sidecars/),
measured on a laptop with 15-minute meetings. `nemotron`, the recommended
one, runs NVIDIA's Nemotron 3 Diarization in `nemo-speech serve`, a single
download with no Python: it takes under a minute for 15 minutes of audio on
the CPU (17 seconds on an Intel GPU), found 4, 4 and 4 voices in the three
four-person meetings, 1 in a video with one teacher and 2 in a two-person
call (beside a 3-second third), and almost never mixes two people up, but
leaves more quiet or short speech without a voice. It ignores
`num_speakers`. `sherpa-onnx` is a small Python server started with `uv`: it
took under 3 minutes for the same audio and gave the most speech the right
voice. Deepgram's own speaker separation (`diarization = "backend"`) needs
no sidecar and comes free with its transcription, but scored clearly worse
than both on the same meetings.

whisrs waits for the sidecar's answer for twice the length of the speech it
sent, and at least 10 minutes. A sidecar that cannot be reached is tried
again like a transcript chunk (5, 15, 30 and 60 seconds apart). The
sidecar's own error (HTTP 500), a crash on the request, no answer in time and
an answer that does not follow the contract are not retried. Whatever
fails, the recording does not: its transcript is written with one speaker
per track, as with `"off"`, `recording.json` notes why under `errors` (the
transcript's notes show it too), and the notification that the transcript
is ready says it has no speakers. The voices the sidecar found are kept in
the recording's `.cache/` folder after the transcript is written, so a retry
with the same sidecar, number of voices and audio reuses them instead of
asking again; they are deleted with the audio.

**Languages.** Each track is transcribed in its own language. `language`
defaults to `[general] language`, and `mic_language` and `system_language`
default to `language`, so dictation and recordings can use different
languages. One recording can override them with `-l` (both tracks),
`--mic-language` and `--system-language`, which win over `-l` for their
track. For language practice against a video, where you repeat lines in
German and the teacher mixes German and English:

```
whisrs recording start -t "Lesson 50" --mic-language de --system-language auto --others Video
```

and for a meeting in another language, `whisrs recording start -l de`. The
languages a recording starts with are saved in its `recording.json` and stay
with it: a later config change does not reach it. To redo a recording in
another language, run `whisrs recording retry <id> -l <language>` (or
`--mic-language`/`--system-language` to change one track); a language you
give replaces the saved one, and a track you leave out keeps its own. The
saved languages are those of the latest attempt: a retry with `-l` that fails
leaves the new languages saved, with the recording marked failed. The
transcript's front matter shows `language` (or `mic_language` and
`system_language` when they differ) and the `model` it was made with.

**Headphones are recommended.** With speakers, the system audio leaks into
your microphone. whisrs filters that echo out of your track before transcribing
it, using the system audio as the reference. Echoed lines that still get
through are dropped, but headphones give a cleaner transcript and avoid the
occasional miss. Echo is also harder to catch when the two tracks are
transcribed in different languages, since a line heard on both no longer
reads the same.

**Dictation is off while a recording is in progress.** The microphone is
shared with the recording, so the transcript would record a dictation as your
speech, and in a call the other people would hear it. Until the recording
stops, `whisrs toggle`, command mode and `[[llm_commands]]` are refused with a
notification saying why. A dictation already running when the recording
started finishes normally, and read-aloud keeps working. The tray icon turns
into a red outlined bubble with a dot while a recording is in progress, and an
amber outlined bubble with three dots while it is transcribed (as long as you
are not dictating at that moment). A recording shows no on-screen overlay and
fires no `[hooks]`, so media that is playing is not paused.

**Backends.** A recording is transcribed from its audio files after it ends,
so it needs a backend that can transcribe a recorded file: groq, openai,
deepgram, asr-sidecar or local-whisper.
`backend` defaults to `[general] backend`. With `deepgram-streaming` the
recording goes to Deepgram's REST API. Groq, Deepgram and OpenAI's `whisper-1`
receive the audio as FLAC, which is lossless and, for speech, usually less than half the size of WAV.
Only the stretches where someone speaks are sent, a second of silence apart, and
the timestamps are mapped back onto the recording. The
realtime backends (`openai-realtime`, `openai-compatible-realtime`) cannot
transcribe a recording: setting one as
`[recording] backend` is a config error. Inheriting one from `[general]` gives a
startup warning when `[recording]` or `[hotkeys] recording` is set, since
recordings would capture audio but fail to transcribe.

With `openai`, recordings use `[openai] model`, except that the realtime-only
models (`gpt-live-transcribe`, `gpt-realtime-whisper`) become `gpt-transcribe`,
since OpenAI's file endpoint refuses them. So an `openai-realtime` dictation
setup can keep its model and still set `[recording] backend = "openai"`.
`[openai] languages` does not apply to recordings; they use their own languages
(see "Languages" above).
Recordings send `[general] vocabulary` as the hint, but not `[general] prompt`,
which is an instruction for dictation. A `local-whisper` recording takes
neither: it decodes without any prompt, so no decoded text is carried
forward within a stretch of speech, which can make whisper repeat itself.

With `local-whisper`, a recording is always decoded in short phrases (at most
about 28 seconds each) whatever `[local-whisper] segmentation` says, `"none"`
included, so long recordings do not fall into whisper's repetition loop.
`flash_attn` applies to recordings as it does to dictation.

**Devices.** `mic_device` defaults to `[audio] device`. `system_device`
defaults to the monitor of your default output device, and the recording
follows it: when the default output changes while it runs (headphones plugged
in or unplugged, another output picked), the system track moves to the new
one within about 2 seconds, and the moment in between is filled with silence
so the track stays in step with your mic. Set it to `"none"` to record only
your mic, or to a specific monitor source from `pactl list short sources`
(names ending in `.monitor`), which is never followed. `whisrs config` lists
the monitors under "Recording". If the system audio cannot be opened, the
recording takes your mic only and notes that in the transcript. With
`mic_device` and `[audio] device` at `"default"`, the mic records the ALSA
default device, which whisrs does not move. Through PipeWire's ALSA plugin
(`pipewire-alsa`, the usual setup) that is PipeWire's default input, and
PipeWire moves it to a new default (a headset plugged in, a USB mic
unplugged) by itself. Through the PulseAudio ALSA plugin, whether it moves
depends on your PulseAudio setup, and on plain ALSA (no sound server) it
stays on the device it opened.

**Where recordings are saved.** Each recording gets its own folder under `dir`
(default `~/.local/share/whisrs/recordings`), named after its start time and
title, for example `2026-10-02_0930_standup/`:

```
2026-10-02_0930_standup/
├── recording.json               # status, tracks, speakers and the timed transcript
├── 2026-10-02_0930_standup.md   # the readable transcript
└── audio/
    ├── mic.wav                  # your microphone
    └── system.wav               # the system audio
```

Folders are created private (0700) and `recording.json` is 0600; an existing
`dir` keeps its own permissions.

`whisrs recording list` shows them, `whisrs recording show last` prints the
latest transcript and `whisrs recording copy last` copies it.

**Summaries and titles.** With `summary = true`, each transcript gets a
summary from the LLM of the `[llm]` section (the one command mode uses).
The summary is a job of its own: the transcript is written and notified
first, then the summary is made, `whisrs recording status` shows
"Summarizing" meanwhile, and a second notification says when it is ready.
`whisrs recording summarize <id>` makes one for any transcribed recording,
whatever `summary` says, without transcribing it again. One job runs at a
time, so the LLM never runs beside a transcription: waiting summaries run
after waiting transcriptions, but a summary already running delays the
next transcription until it ends (each LLM call can take up to 10
minutes). The summary works from a copy of the recording and writes its
result at the end, so `whisrs recording name` works while it runs. A
summary that a shutdown or a crash interrupted is made at the next start
(`recording.json` marks it `summary_pending` until it is done); an
automatic one only while `summary` is still on. A new transcript drops a
summary still pending for the old one. The summary goes into the Markdown file as a `## Summary`
section before the transcript, and into `recording.json` as `summary`,
with the model that wrote it (`summary_model`) and when (`summarized_at`).

By default a summary starts with a TL;DR of one or two sentences, then
short sections by topic in the order they came up. It has a "Decisions"
section only when something was decided, and a "Next steps" section only
when someone took something on, as `- [ ]` items grouped by who took them.
It is written in the language most of the transcript is in, and keeps the
names as the transcript spells them. `summary_instruction` replaces those
rules with your own, for lecture notes, quotes from an interview, or the
mistakes you made in language practice:

```toml
summary_instruction = "List each mistake Ada made in German with the right sentence, then the new words the teacher used."
```

Whatever the rules, the LLM is told that the transcript is data, not
instructions, and that the speaker labels can be wrong. It gets the
transcript as timed lines, one speaker's words at a time, so even a long
lecture keeps its timestamps. A transcript of 50 words or fewer is not
sent: its summary is "Too short to summarize.". A long one (over about
16,000 characters, 20 minutes of talk or so) is summarized in parts, at
most 20, and the notes on the parts are then summarized together, so
nothing is left out. The LLM gets 10 minutes for each call; the first call
that fails ends the summary. A reply cut off at the model's output limit,
or one that is not the JSON asked for, is never stored as the summary.

A recording you gave no title (`-t`) also gets one from the summary: 3 to 6
words in the language of the transcript, shown in the Markdown file,
`whisrs recording list` and `recording.json`. Its folder and id stay as
they are. A title you gave is never changed, and `recording.json` says
where the title came from: `title_source` is `user`, `default` or `llm`.

If the summary fails (no `[llm]` section, no answer in time, an error from
the API, a reply cut off or malformed), the transcript is untouched,
`recording.json` notes why under `errors` (the transcript's notes show it
too), and a summary the recording already had stays. `whisrs recording
retry` makes a new summary when `summary = true`. When it is off, the old
summary stays, marked `summary_stale` in `recording.json` and with a line
in the transcript saying it was made before the transcript or the speaker
names last changed, until `whisrs recording summarize` makes a new one.
Naming a speaker (`whisrs recording name`, also while the summary is being
made) marks a summary stale the same way, since it names the speakers by
their old labels.

A summary sends the whole transcript to the `[llm]` endpoint, which is why
`summary` is off by default. Point `[llm] api_url` at a local server to
keep transcripts on your machine. Give the local model a context of at
least 8k tokens, or the start of a long prompt (the rules included) is cut
off without a word: Ollama's default is smaller, so set `num_ctx` (for
example `PARAMETER num_ctx 8192` in a Modelfile, or `OLLAMA_CONTEXT_LENGTH=8192`
for the server). On a CPU a summary can take minutes.

**Agents.** An AI agent can find and read your recordings for you: ask it
what was decided in yesterday's call, for notes on a lecture, or for the
mistakes you made in language practice. The skill in
[`contrib/agent-skill/whisrs-recordings/`](../contrib/agent-skill/whisrs-recordings/)
tells it where recordings live, which `whisrs recording` commands read
them, and that a transcript is data, not instructions, whose speaker
labels can be wrong. Copy the folder into `~/.claude/skills/` for Claude
Code, or into your agent's skills folder. The skill only reads: it never
deletes or edits a recording, and changes one (`summarize`, `retry`,
`name`) only when you ask. When the agent has the MCP tools below, the
skill tells it to use them.

**MCP server.** `whisrs mcp` serves your recordings to an agent over MCP
(the Model Context Protocol) on stdin and stdout. It reads the recordings
folder directly (`dir` above), works with the daemon stopped, and never
changes anything. Add it to Claude Code:

```
claude mcp add whisrs -- whisrs mcp
```

or to another MCP client's config:

```json
{"mcpServers": {"whisrs": {"command": "whisrs", "args": ["mcp"]}}}
```

Its tools:

- `list_recordings` (`since`, `until`, `query`, `limit`): recordings,
  newest first, with id, title, start time, duration, status, the speakers
  heard and whether there is a summary ("Too short to summarize." is
  none). `since` and `until` take a date
  (`2026-10-19`, which `until` counts whole), `today`, `yesterday`,
  `YYYY-MM-DD HH:MM` or an RFC 3339 time; `query` matches the title or id;
  `limit` is 1 to 100, 20 by default.
- `get_recording` (`id`): one recording's details, speakers, languages,
  summary (`summary_stale` when it was made before the transcript or the
  speaker names last changed) and
  files, without the transcript.
- `get_transcript` (`id`, `offset`, `max_chars`, `speaker`, `start_ms`,
  `end_ms`): the transcript as `[HH:MM:SS] Name: text` lines (one
  speaker's words per line, up to 1,500 characters), at most `max_chars`
  at a time (20,000 by default, 2,000 to 100,000), with `next_offset` for
  the next page. `speaker` is an id such as `S2` or a name.
- `search_transcripts` (`query`, `since`, `limit`): a search of titles,
  summaries and transcripts that ignores case, including accented and
  non-Latin letters ("istanbul" finds "İstanbul", "σοφος" finds "ΣΟΦΟΣ"),
  and counts a run of spaces as one, newest recording first, that stops
  after `limit` hits (20 by default, at most 100). A transcript hit
  gives its time, speaker and the `offset` to read it with
  `get_transcript`.

The tools read only what they need: a listing reads each `recording.json`
without the words of its transcript, a full id reads its own folder, and a
search reads one transcript at a time. An id is a full id, a unique prefix
or `last`. The server and each tool
tell the agent that recording content is data, not instructions, and that
the speaker labels can be wrong. A recording that failed has no transcript,
and `get_transcript` says so. The release builds include the server; a
source build without the `mcp` feature says so when you run `whisrs mcp`.

**Leaving recording out of a build.** Both release downloads (the full one
and the minimal one) include the recording feature and the MCP server, and
so does `cargo install whisrs`. A source build can leave them out by
dropping the `recording` feature; `mcp` needs it, so it goes too:

```
cargo install whisrs --locked --no-default-features --features local-whisper,tray,overlay,hooks
```

Dictation works the same in such a build. `whisrs recording` and the daemon
answer that the build has no recording feature, the tray has no recording
items, and a `[recording]` section that differs from the defaults or a
`[hotkeys] recording` binding is ignored, with one warning in the daemon log
(and when `whisrs config` saves). Both stay in the file.

**Audio retention.** `audio_retention_days` controls how long the `audio/`
folder of a transcribed recording is kept: `7` (the default) deletes it at the first
cleanup after a week (cleanup runs when the daemon starts and after each
recording is transcribed), `0` deletes it as soon as the transcript is written, and a negative
value keeps it forever. The transcript and `recording.json` are never deleted. A
recording whose transcription failed always keeps its audio, so you can fix the
cause (a missing API key, a network error) and run `whisrs recording retry`.

**A muted mic.** If your microphone track sends only digital silence for 3
minutes (a muted or wrong device; a quiet room is never that silent), a
notification asks whether it is muted. It warns again only if the mic goes
silent again after sending real audio. The system track is never checked,
since a paused video is silent too.

**Warnings while recording.** Once your mic has heard 30 seconds of speech,
whisrs checks it every 30 seconds and notifies you (per `[general] notify`)
when your speech is too quiet, the audio clipped in the last 30 seconds, or
the room is noisy. After 2 minutes of your speech with only digital silence
on the system track it says "Recording: nothing has played on the system
track yet. If this is a call, check [recording] system_device." (a solo memo
can trip it). Each of these comes at most once per recording, and each is
logged too.

**Audio quality.** Each track is measured as it is recorded, before echo
removal: the speech level, the background level, the SNR (how far the speech
is above the background), the share of clipped samples and any DC offset.
`recording.json` keeps that per track under `quality`, with the speech
detector that measured it (`vad`) and, on the mic, `echo_path_db` (how loud
the speakers reach your mic) when echo removal ran. Its `warnings` list what
was off over the whole track: `too-quiet` (speech under -55 dBFS), `noisy`
(speech less than 10 dB above the background), `clipping` (0.1 % or more of
the samples at the limit), `dc-offset` (an offset of 1 % or more of full
scale), `strong-echo` (an echo path above -15 dB) and `system-silent` (the
warning above). A recording from an older whisrs, or one recovered after a
crash, is measured from its audio when it is transcribed or retried. The
transcript's front matter shows the levels of each measured track after
`model`, and `audio_warnings` only when a track warned:

```yaml
audio:
  mic: {speech_dbfs: -41, noise_dbfs: -62, snr_db: 21}
  system: {speech_dbfs: -24, noise_dbfs: null, snr_db: null}
audio_warnings:
  - "mic: noisy (speech 7 dB above the background)"
```

`null` is a level that could not be measured: a track with no speech has no
speech level, and a system track whose pauses are digital silence has no
background.

**Disk space.** A recording takes about 1.9 MB a minute per track, and
transcribing it writes an echo-cancelled copy of your track next to them. A
recording needs 100 MB free in `dir` to start (about 10 minutes plus room to
transcribe them). While it runs, the free space is checked every minute: one
notification warns when less than 30 minutes fit, and with less than 2
minutes left the recording stops, as at the time limit, and is transcribed.
A check that gets no answer within 5 seconds (a hung network mount) is
skipped, so it never delays the time limit.

`max_duration_minutes` (default 180, at most 2160) stops a recording that was
left running. A recording interrupted by a crash or reboot is recovered from its
audio the next time the daemon starts. If its transcription is interrupted three
times in a row (for example because it keeps crashing the daemon), it is marked
failed instead of being resumed again, so dictation keeps working; run
`whisrs recording retry` to try once more.

Cloud backends that answer "rate limited" (HTTP 429) or "overloaded" (503) are
retried with a backoff, honoring the server's `Retry-After`, for up to five
minutes per chunk. A server error (500, 502, 504, or Cloudflare's 520 to 524,
such as a 524 timeout on a long upload) is retried three times, 5, 15 and 30
seconds apart, except a 500 from `asr-sidecar`, which is usually an error in
the sidecar itself. Each finished chunk is saved in the recording's `.cache/`
folder, so a retry after a failure only sends what was not transcribed yet.
The chunks are deleted once the transcript is written, so a retry of a
finished recording sends both tracks again (a diarization sidecar's voices
stay, see "Speakers").

**No network.** A chunk that gets no answer at all (no network, a name that
does not resolve, a refused or dropped connection, a TLS error) is retried 5,
15, 30 and 60 seconds apart. If the network is still down after that, the
recording is not failed: it waits for the network, shown as `waiting` by
`whisrs recording list` and `whisrs recording status`, with one notification.
The daemon tries it again when it starts and every 10 minutes while it runs,
for up to 24 hours from its first failed try (144 tries at most), however
long ago the recording stopped, and then marks it failed. These tries do not
count towards the three interrupted attempts above. `whisrs recording retry`
tries a waiting recording at once, and a retry that finds no network starts
a new 24 hours. An `asr-sidecar` that cannot be reached (not running, a
network that is down, a request that times out) counts as no network, but
one that takes a chunk and then closes the connection crashed on it, so like
its HTTP 500 that fails the recording without sending the chunk again.
`local-whisper` never waits.

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

## Word replacements

`vocabulary` only hints the model. When it still mis-hears a term the same way
every time, map the wrong text to the right one:

```toml
[replacements]
"package build" = "PKGBUILD"
"whisper s" = "whisrs"
```

Every dictation is rewritten before it is typed, copied or logged, and so is
every line of a recording's transcript. Keys match
ignoring case, on whole words, with any spaces or hyphens between the words,
so "Package-build" matches too. Quote every key: unquoted,
`node.js = "Node.js"` is a nested table in TOML, so config.toml fails to
parse and the daemon falls back to built-in defaults. Surrounding punctuation is kept
("package build." becomes "PKGBUILD."). Values are inserted exactly as
written. Text is scanned left to right: where several keys start at the same
word the longest one wins, and a replacement is never itself replaced again.
An empty value deletes the match and the spaces after it, but not the
punctuation next to it. Leave filler words to `remove_filler_words`, which
takes their commas too. Command mode and `[[llm_commands]]` instructions are
left alone. With `llm_post_process` on, replacements run first, so the LLM
sees the corrected text (and can still rewrite it).

On streaming backends the text is rewritten one typed chunk at a time. With
`local-whisper` phrase segmentation or `openai-compatible-realtime` a chunk is
a whole phrase, so this only misses if you pause in the middle of a key.
`deepgram-streaming` sends only final results, which end between words: it can
split a multi-word key across two chunks where Deepgram closes a segment
(usually a pause), in which case it does not match, but never cuts a word.
`openai-realtime` streams tokens, so a chunk can also end in the middle of a
word, and a key like "pack" can match the first half of "package". Batch
backends see the whole transcript and are not affected.

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
