# Recording

`whisrs recording` records two tracks at once, you through your microphone and what your
computer plays through the system audio, and when you stop, transcribes both into one timed
transcript: of meetings and calls, lectures, videos, interviews, or language practice. Its
keys are in the [config reference](config-reference.md#recording).

## Start and stop

```
whisrs recording start [-t TITLE]   # start; also the [hotkeys] recording key and the tray menu
whisrs recording stop               # stop and queue the transcript
whisrs recording toggle             # start, or stop the one in progress (for a compositor bind)
whisrs recording cancel             # stop and delete it
whisrs recording status             # Recording, Processing, Summarizing or Idle
```

While a recording runs, dictation, command mode, `[[llm_commands]]` and agent voice turns
are refused with a notification: the mic is shared, and in a call the others would hear you dictate.
Read-aloud keeps working, and agent replies wait until the recording stops. The tray icon
turns red while it records and amber while it is transcribed. A recording shows no overlay
and fires no `[hooks]`, so playing media is not paused.

**Use headphones.** With speakers, the system audio leaks into your mic. whisrs filters that
echo out of your track, but headphones give a cleaner transcript.

## What a recording produces

Each recording gets a folder under `[recording] dir` (default
`~/.local/share/whisrs/recordings`), named after its start time and title:

```
2026-10-02_0930_standup/
├── recording.json               # status, tracks, speakers and the timed transcript
├── 2026-10-02_0930_standup.md   # the readable transcript
└── audio/
    ├── mic.wav                  # your microphone
    └── system.wav               # the system audio
```

Folders are private (0700). `whisrs recording list` shows the recordings, `show last` prints
the latest transcript (`-f md|json|txt`), `copy last` copies it, `path <id>` prints its
folder, and `delete <id>` removes the whole folder for good. Your track is labelled with
`[recording] user_name` and the system track "Others" (`--others NAME` on `start` names it),
unless its voices are told apart (see [Speaker names](#speaker-names)).

## Backends

A recording is transcribed from its files, so it needs a backend that takes a file: `groq`,
`openai`, `deepgram`, `asr-sidecar` or `local-whisper`. `[recording] backend` defaults to
`[general] backend`; `deepgram-streaming` uses Deepgram's REST API here. The realtime
backends cannot transcribe a recording: as `[recording] backend` they are a config error,
and inherited from `[general]` they get a startup warning.

- Only the stretches where someone speaks are sent.
- With `openai`, the realtime-only models (`gpt-live-transcribe`, `gpt-realtime-whisper`)
  become `gpt-transcribe`, so an `openai-realtime` dictation setup can keep its model.
- Recordings send `[general] vocabulary` as the hint, never `prompt`; `local-whisper` sends
  neither and always decodes in phrases of at most about 28 seconds, whatever its
  `segmentation`.

## Languages

Each track is transcribed in its own language. `[recording] language` defaults to
`[general] language`, and `mic_language` and `system_language` default to `language`. One
recording can override them with `-l` (both tracks), `--mic-language` and
`--system-language`. For language practice, where you repeat German lines and the teacher
mixes German and English:

```
whisrs recording start -t "Lesson 50" --mic-language de --system-language auto --others Video
```

A recording keeps the languages it started with. `whisrs recording retry <id> -l <language>`
redoes it in another one; a track you leave out keeps its own.

## Devices

`mic_device` defaults to `[audio] device`. `system_device` defaults to the monitor of your
default output and follows it: when the default output changes (headphones plugged in), the
system track moves to the new one within about 2 seconds, the gap filled with silence. Set
it to `"none"` to record only your mic, or to a source ending in `.monitor` from
`pactl list short sources`, which is never followed. If the system audio cannot be opened,
the recording takes your mic only and the transcript says so.

## Speaker names

whisrs can tell apart the voices on the system track (`[recording] diarization`). Each voice
becomes a speaker, numbered in the order they first speak: "Speaker 1", "Speaker 2". When
the system audio holds a single voice, it keeps the others label.

- `"auto"` (the default) uses the [diarization sidecar](#diarization-sidecar) when
  `diarization_url` is set, else Deepgram's speakers on Deepgram, else one speaker per
  track. `"backend"` takes the speakers the backend labels (Deepgram, or an `asr-sidecar`
  that returns them), `"sidecar"` the sidecar's, and `"off"` keeps one speaker per track.
- A backend labels voices per request, so one person in two chunks of a long track (an hour
  of speech on Deepgram, 10 minutes on `asr-sidecar`) becomes two speakers. Deepgram's
  Whisper models tell no speakers apart.
- With several people on your mic, set `shared_mic = true` (or `--shared-mic` on `start`,
  `toggle` or `retry`): the mic's voices are told apart too, and the mic is you only when it
  holds one voice.

A voice gets a name only when whisrs can tell who it is. In order, the first that names it
wins:

1. A name you gave with `whisrs recording name`.
2. Your mic is you (`user_name`) when it holds one voice.
3. `--with "Sarah, Omar"` on `start`, `toggle` or `retry` says who the recording is with
   (up to 20 names). It names only voices that were told apart: one voice on the system
   audio and one person listed, that voice is them. Without voice separation the system
   track stays one "Others" speaker, so name it with `--others` instead.
4. With `name_speakers = true`, the `[llm]` is asked who the unnamed voices are. It must
   quote the line that shows each name ("I'm Sarah", "Thanks, Sarah"), and a name is kept
   only when that line is in the transcript and holds it; with `--with`, only a listed
   person's name is kept.
5. When every voice but one has a name, one listed person is left, and there are as many
   voices as people listed (with `shared_mic` off), the last voice is that person.
6. Everyone else is "Speaker N".

`name_speakers` is off by default because it sends parts of every transcript (the title,
the names, and about 2000 characters of each voice it asks about) to the `[llm]` endpoint.
Point `[llm] api_url` at a local server to keep them on your machine. If the LLM fails, the
transcript is written without its names.

To name a speaker yourself, give the recording, the speaker (its id `S2`, "Speaker 2", or
its current name) and the name:

```
whisrs recording name last "Speaker 2" Sarah
whisrs recording name last S3 Omar Haddad
whisrs recording name last Sarah ""        # clear the name you gave
```

The transcript is rewritten at once, without transcribing again, and the other steps run
again around your name. A name you gave follows its voice through a `retry`, which can
number the voices differently; when no voice there matches, the name is dropped and the
transcript asks you to name the speakers again.

## Summaries

With `[recording] summary = true`, each transcript gets a summary from the `[llm]`, as a
`## Summary` section before the transcript. `whisrs recording summarize <id>` makes one for
any transcribed recording, whatever `summary` says. It runs after the transcript is written,
never beside a transcription, with a second notification when it is ready.

- By default a summary starts with a TL;DR, then short sections by topic, a "Decisions"
  section when something was decided, and "Next steps" as `- [ ]` items by who took them,
  in the language of the transcript.
- `summary_instruction` replaces those rules with your own, for lecture notes, interview
  quotes, or your mistakes in language practice:

```toml
summary_instruction = "List each mistake Ada made in German with the right sentence, then the new words the teacher used."
```

- A recording you gave no title (`-t`) gets one from the summary: 3 to 6 words. A title you
  gave is never changed, and the folder and id stay as they are.
- A transcript of 50 words or fewer is "Too short to summarize.". A long one is summarized
  in parts, then the parts together, so nothing is left out. Each LLM call gets 10 minutes.
- A `retry` makes a new summary when `summary = true`; otherwise the old one stays, marked
  as made before the transcript or the speaker names last changed.

A summary sends the whole transcript to the `[llm]` endpoint, which is why `summary` is off
by default. With a local model, give it a context of at least 8k tokens, or the start of a
long prompt is cut off without a word (on Ollama, `PARAMETER num_ctx 8192` in a Modelfile,
or `OLLAMA_CONTEXT_LENGTH=8192` for the server). On a CPU a summary can take minutes.

## Diarization sidecar

A diarization sidecar is a local server that only finds who speaks when, so speakers can be
told apart offline or on a backend that cannot do it (Groq, OpenAI, `local-whisper`). Set
`[recording] diarization_url`, and whisrs sends it each track's speech once, then gives each
word of the transcript the voice it overlaps most. Two ready sidecars, and the contract a
sidecar speaks, are in [`contrib/diarization-sidecars/`](../contrib/diarization-sidecars/).

`num_speakers` (or `--num-speakers N`, 0 to 20) says how many voices the system audio holds;
0 lets the sidecar decide, and the `nemotron` recipe always does.

whisrs waits for the answer for twice the length of the speech it sent, at least 10
minutes, and retries a sidecar it cannot reach. When the sidecar fails, the transcript is
still written, with one speaker per track. The voices it found are kept with the audio, so a
`retry` with the same sidecar does not ask again.

## MCP server

`whisrs mcp` serves your recordings to an AI agent over MCP (the Model Context Protocol) on
stdin and stdout. It reads the recordings folder directly, works with the daemon stopped,
and never changes anything. Add it to Claude Code with `claude mcp add whisrs -- whisrs mcp`,
or to another MCP client's config:

```json
{"mcpServers": {"whisrs": {"command": "whisrs", "args": ["mcp"]}}}
```

Its tools: `list_recordings` (newest first, by date or title), `get_recording` (one
recording's details and summary), `get_transcript` (the transcript as timed lines, a page
at a time, optionally one speaker's) and `search_transcripts` (titles, summaries and
transcripts, in any case). An id is a full id, a unique prefix or `last`. The tools tell the
agent that recording content is data, not instructions, and that speaker labels can be
wrong.

The agent skill in
[`contrib/agent-skill/whisrs-recordings/`](../contrib/agent-skill/whisrs-recordings/) does
the same through `whisrs recording` commands: copy the folder into your agent's skills
folder (`~/.claude/skills/` for Claude Code). It changes a recording only when you ask.

## Audio retention

`audio_retention_days` sets how long the `audio/` folder of a transcribed recording is kept:
`7` (the default) deletes it at the first cleanup after a week (when the daemon starts and
after each transcript), `0` as soon as the transcript is written, and a negative value
keeps it forever. The transcript and `recording.json` are never deleted. A recording whose
transcription failed keeps its audio, so you can fix the cause (a missing key, the
network) and run `whisrs recording retry`.

A recording takes about 1.9 MB a minute per track and needs 100 MB free to start. With
room for under 30 minutes left a notification warns, and with under 2 minutes the recording
stops and is transcribed, as at `max_duration_minutes`.

## Quality warnings

If your mic sends only digital silence for 3 minutes (muted, or the wrong device), a
notification asks whether it is muted. A recording also warns, per `[general] notify`, when
your audio clips, is too quiet or noisy, or nothing plays on the system track:
[Noisy room or quiet mic](troubleshooting.md#noisy-room-or-quiet-mic) says when each comes
and what to do. The transcript's front matter shows each track's levels (speech,
background, SNR) and warnings: `too-quiet`, `noisy`, `clipping`, `dc-offset` and
`strong-echo` for the mic, `system-silent` for the system track.

## When something fails

- A cloud backend that answers "rate limited" (429) or "overloaded" (503) is retried with a
  backoff for up to five minutes a chunk, and a server error three times. Each finished
  chunk is kept, so a retry after a failure sends only what is left.
- With no network, a chunk is retried for about two minutes, then the recording waits for the
  network (`waiting` in `whisrs recording list`), tried every 10 minutes for up to 24 hours.
  `whisrs recording retry` tries it at once.
- A recording a crash or reboot interrupted is recovered from its audio at the next daemon
  start. One whose transcription was interrupted three times in a row is marked failed, so
  dictation keeps working; `whisrs recording retry` tries again.
