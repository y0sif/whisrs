# nemo-speech ASR sidecar

Two NVIDIA speech-to-text models, served by
[NeMo-Speech.cpp](https://github.com/NVIDIA/NeMo-Speech.cpp) (`nemo-speech serve`),
the same single prebuilt binary (no Python) as the
[Nemotron diarization recipe](../../diarization-sidecars/nemotron/). Its server
speaks OpenAI's `/v1/audio/transcriptions` and a realtime WebSocket, so whisrs
talks to it as it is, with nothing to wrap. One server per model:

| Port | Model | In whisrs |
|---|---|---|
| 8772 | [Nemotron 3.5 ASR Streaming 0.6B](https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b) | live dictation (`openai-compatible-realtime`) and recordings (`asr-sidecar`); batch dictation too, but see Known limits |
| 8773 | [Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) | batch dictation only (`asr-sidecar`) |

- Runtime: NeMo-Speech.cpp 0.2.0, Apache-2.0.
- Models: 8-bit (q8_0) GGUF model files of 742 MB (Nemotron, OpenMDW-1.1)
  and 714 MB (Parakeet, CC-BY-4.0). Not gated: no account or token needed.

Use Parakeet for batch dictation: it is the fastest local batch model here,
and it never dropped a word in our runs, while Nemotron can drop the first
words of a quiet voice (see Known limits). Use Nemotron for live dictation
and for recordings. whisrs sends a recording's first chunk (up to 10 minutes
of speech) whole, and only Nemotron's memory stays bounded on that: under the
unit's 4G memory cap, a recording with more than about 5 minutes of speech
fails on Parakeet (nemo-speech 0.2.0), and a retry fails the same way. The numbers are below.

## Measured on a laptop

The CPU build on 4 cores of an Intel Core Ultra 9 185H, with one 8.4-second
English clip:

| | Nemotron 3.5 ASR Streaming | Parakeet TDT v3 |
|---|---|---|
| Batch request (`asr-sidecar`) | 2.9 to 3.4 s | 2.0 to 2.3 s |
| Live: a phrase arrives | about 1 s after you stop speaking | no live mode |
| Live: the last phrase arrives | 0.46 s after you press stop | |

Both heard "whisrs" as "Wizards" or "Wizard's" and "Hyprland" as "Hyperland"
(see Known limits); Parakeet punctuated best on that clip. On a real 21.6 s mic recording
of a quiet voice, Parakeet got the most words right but returned no
punctuation, and Nemotron dropped the first 3.4 s of speech.

Long audio, one file per request (as whisrs sends a recording's first chunk),
on the same 4 cores under the units' `MemoryMax=4G` with no swap. Peak memory,
then time:

| Audio | Parakeet TDT v3 |
|---|---|
| 120 s | 2.06 GB, 36 s |
| 240 s | 2.53 GB, 99 s |
| 300 s | 3.56 GB, 140 s |
| 360 s | killed at the cap |
| 600 s | killed at the cap |

Parakeet's memory grows faster than the audio, and nemo-speech 0.2.0 and
0.3.0 have no option to bound it. Nemotron stays low but takes about real
time. Nemotron on 600 s: 1.44 GB, 549 s.

nemo-speech 0.3.0 works with the same configs and flags and uses less memory
(Parakeet 2.92 GB on 300 s, and 360 s finished under the cap at 4.04 GB;
Nemotron 1.27 GB on 600 s). This recipe stays on 0.2.0, the diarization
recipe's runtime.

## Install

1. The runtime (skip this if the diarization recipe already installed it).
   Pick the CPU build, or the Vulkan build (`build=vulkan`) for a GPU with a
   Vulkan driver (`vulkan-intel`, `vulkan-radeon` or the NVIDIA driver).

   ```sh
   mkdir -p ~/.local/share/nemo-speech && cd ~/.local/share/nemo-speech
   build=cpu   # or: build=vulkan
   base=https://github.com/NVIDIA/NeMo-Speech.cpp/releases/download/v0.2.0
   curl -LO "$base/nemo-speech-0.2.0-linux-x86_64-$build.tar.gz"   # cpu 6.7 MB, vulkan 16.6 MB
   sha256sum --check --ignore-missing <<'EOF'
   f396057150f1b774935c7414fd32ebc04195a795a34120d78d8c9ecefa1b7507  nemo-speech-0.2.0-linux-x86_64-cpu.tar.gz
   c8d0ed5b9ac5fb6d5c460015ba38ea217415c44edc1e6d49120646781009aee6  nemo-speech-0.2.0-linux-x86_64-vulkan.tar.gz
   EOF
   tar xzf "nemo-speech-0.2.0-linux-x86_64-$build.tar.gz"
   ```

   `sha256sum` must print `OK` for the file you downloaded.

2. Nemotron, pinned to the revision measured here, by its Hugging Face URL:

   ```sh
   cd ~/.local/share/nemo-speech
   curl -LO https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b/resolve/ea30d66debe3740a08b573244286791d423d6b3e/nemotron-3.5-asr-streaming-0.6b.q8_0.gguf
   echo "3fc991d3badad7277c11030a7519832cddaf2057aafed6d4b25147e953a070b1  nemotron-3.5-asr-streaming-0.6b.q8_0.gguf" | sha256sum --check
   ```

   Not `nemo-speech pull`: it cannot pick a revision, and its index (in 0.2.0
   and 0.3.0) pins an older file, `1c8deae`, that was not measured here.

3. Parakeet, for batch dictation, through `nemo-speech pull`, which
   checks the size and sha256 itself:

   ```sh
   NEMO_SPEECH_MODEL_DIR=~/.local/share/nemo-speech/models \
     ~/.local/share/nemo-speech/nemo-speech-0.2.0-linux-x86_64-cpu/bin/nemo-speech pull parakeet-tdt
   ```

   With the Vulkan build, the binary is in
   `nemo-speech-0.2.0-linux-x86_64-vulkan/bin/`. It prints where the file
   landed:
   `~/.local/share/nemo-speech/models/nvidia/parakeet-tdt-0.6b-v3/541d1f99c6b0c3cd0b11a95167540bb8edefd82b/parakeet-tdt-0.6b-v3.q8_0.gguf`
   (the index pins revision `541d1f9`, the one measured here).

## Run

Each runs in the foreground: start each in its own terminal.

```sh
# Nemotron, port 8772
~/.local/share/nemo-speech/nemo-speech-0.2.0-linux-x86_64-cpu/bin/nemo-speech serve \
  --host 127.0.0.1 --port 8772 --no-ui --device cpu \
  --asr-model ~/.local/share/nemo-speech/nemotron-3.5-asr-streaming-0.6b.q8_0.gguf \
  --endpointing

# Parakeet, port 8773 (batch dictation only)
~/.local/share/nemo-speech/nemo-speech-0.2.0-linux-x86_64-cpu/bin/nemo-speech serve \
  --host 127.0.0.1 --port 8773 --no-ui --device cpu \
  --asr-model ~/.local/share/nemo-speech/models/nvidia/parakeet-tdt-0.6b-v3/541d1f99c6b0c3cd0b11a95167540bb8edefd82b/parakeet-tdt-0.6b-v3.q8_0.gguf
```

`--endpointing` makes Nemotron end a phrase at each pause in your speech, so
live dictation types it then. Without it the server sends no phrase until
whisrs stops the stream: nothing is typed while you talk, and everything at
once after you stop. The flag is not in `serve --help`; it is an alias of
`--asr.endpointing.enable=true`. Parakeet has no live mode and needs no flag.

With the Vulkan build, use its directory and `--device vulkan:0`; the numbers
above are from the CPU build. Keep `--host 127.0.0.1`: anyone who can reach
the port can use the server.

To start them with your session instead, install the user units
[`nemotron-asr.service`](nemotron-asr.service) and
[`parakeet-asr.service`](parakeet-asr.service) (edit `ExecStart` for the
Vulkan build), and enable the ones you use. From a whisrs checkout at
`~/whisrs`:

```sh
install -Dm644 ~/whisrs/contrib/asr-sidecars/nemo-speech/nemotron-asr.service ~/.config/systemd/user/nemotron-asr.service
install -Dm644 ~/whisrs/contrib/asr-sidecars/nemo-speech/parakeet-asr.service ~/.config/systemd/user/parakeet-asr.service
systemctl --user daemon-reload
systemctl --user enable --now nemotron-asr.service   # and parakeet-asr.service if you use it
```

Each unit caps its sidecar at `MemoryMax=4G` (with `MemorySwapMax=0`). Past
it the kernel stops the sidecar alone, not your session, and whisrs fails
that dictation or recording.

## Check it

```sh
curl -s http://127.0.0.1:8772/ready
ffmpeg -y -loglevel error -i some-talk.m4a -t 20 -ac 1 -ar 16000 /tmp/talk.wav
curl -s -F file=@/tmp/talk.wav -F model= http://127.0.0.1:8772/v1/audio/transcriptions
```

The answer is `{"text":"..."}`. Use port 8773 for Parakeet.

## whisrs config

### Batch dictation

Use Parakeet (8773). Nemotron (8772) works too, but can drop your first words
(see Known limits).

```toml
[general]
backend = "asr-sidecar"
language = "en"

[asr-sidecar]
url = "http://127.0.0.1:8773/v1/audio/transcriptions"   # 8772 for Nemotron
model = ""
```

Keep `model = ""`, which means "the loaded model". nemo-speech 0.2.0 ignores
the model id, but 0.3.0 answers 404 (`unknown_model`) for any id other than
the loaded model's (`GET /v1/models` lists it), and that includes `nemotron`
and whisrs' default, `microsoft/VibeVoice-ASR-HF`.

Recordings go to Parakeet too (they use `[general] backend` when
`[recording] backend` is unset, and there is one `[asr-sidecar] url`), and
it fails one with more than about 5 minutes of speech. Set
`[recording] backend` to `groq`, `openai`, `deepgram` or `local-whisper`,
point the url at Nemotron (8772) for both, or dictate live on Nemotron (the
last config below).

### Live dictation

```toml
[general]
backend = "openai-compatible-realtime"

[openai-compatible-realtime]
url = "ws://127.0.0.1:8772/v1/audio/transcriptions/realtime"
profile = "lemonade"
model = "nemotron"   # any name: whisrs needs one, the realtime endpoint ignores it
```

The server needs `--endpointing` (see Run) for text to appear as you talk.
That flag decides when a phrase ends, not `[openai-compatible-realtime]
turn_detection`.

`[general] silence_timeout_ms` (2000 by default) still ends the whole
dictation at a pause that long. To pause longer, raise it, or set `0` to stop
only on a second press.

This config alone cannot transcribe recordings. With a `[recording]` section
or a `[hotkeys] recording` binding, whisrsd warns at startup that recordings
cannot use a realtime backend; the next config fixes it.

### Live dictation and recordings on Nemotron

A recording cannot use a realtime backend, so it goes to the same Nemotron
server through `asr-sidecar` while dictation stays live. One server serves
both, the two jobs Nemotron is recommended for.

```toml
[general]
backend = "openai-compatible-realtime"

[openai-compatible-realtime]
url = "ws://127.0.0.1:8772/v1/audio/transcriptions/realtime"
profile = "lemonade"
model = "nemotron"

[asr-sidecar]
url = "http://127.0.0.1:8772/v1/audio/transcriptions"
model = ""

[recording]
backend = "asr-sidecar"
```

A recording is transcribed at about real time on 4 CPU cores: the whole-chunk
request (see Known limits) took 549 s on a 10-minute file, and the phrase
requests come after it, so a 10-minute recording takes more than 9 minutes
after the stop. Live dictation keeps working meanwhile, but slower: with both on
the same 4 cores, a phrase arrived about 10 s after the pause instead of 1 s, and
the last one about 19 s after the stop.

## Known limits

- **Vocabulary and `[general] prompt` do nothing.** whisrs sends them as the
  `hotwords` field, and on the live profile not at all; nemo-speech boosts
  words only from its own `speech_contexts` field, and Parakeet TDT has no
  boosting. For a word it always mishears, use
  [`[replacements]`](../../../docs/configuration.md#word-replacements).
- **Live dictation sends no language.** whisrs' `lemonade` profile sends the
  server only the model name and the turn detection, so `[general] language`
  and `whisrs toggle -l` do not reach it, and nemo-speech uses its own
  default. Only English was tested live.
- **A wrong language code is no error.** In batch, Nemotron falls back to
  English (`en-US`) for a code it does not know, without saying so.
- **A phrase said twice in one live dictation is typed once.** whisrs skips a
  finished phrase whose text matches one it already typed in that dictation.
  That is a whisrs limit on every `lemonade` server, not a nemo-speech one.
- **Live phrases end without a period.** Nemotron punctuates inside a live
  phrase but not at its end, and whisrs joins phrases with a space, so
  sentences run on between pauses. Batch requests end with a period.
- **Nemotron can drop your first words.** A quiet voice (about -47 dBFS)
  right after near-silence, such as a mic fading in from zero, lost its first
  words (3.4 s of a real recording), in batch and live, on 0.2.0 and 0.3.0.
  None were lost after room noise or from a loud voice; Parakeet lost none.
- **Recordings go phrase by phrase.** No `segments` come back, so whisrs
  discards the text of the first chunk sent whole (pure extra time) and sends
  each phrase alone, which costs accuracy: sent alone, a phrase came back as
  "Northern splaying, not playing a video"; sent whole, "Nothing is playing
  and not playing a video".
- **Parakeet has no live mode.** Parakeet TDT has no streaming endpointing,
  so it serves batch requests only.
