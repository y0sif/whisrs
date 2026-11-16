<!-- Generated from src/cli/tui/schema.rs by src/cli/tui/reference.rs: edit those, then run WHISRS_UPDATE_DOCS=1 cargo test. -->

# Config reference

Every key of `~/.config/whisrs/config.toml`, with its default. `whisrs config` edits them all; [configuration.md](configuration.md) explains how the features fit together.

A key left out takes its default; "unset" says what applies then. A key that needs a build feature does nothing in a build without it: the default build has every feature, and the minimal release build has all but `local-whisper`.

## [general]

Guide: [Batch and streaming backends](configuration.md#batch-and-streaming-backends).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `language` | string | `en` | `auto`, or a language code (`en`, `pt-BR`) | The language you dictate in, or auto. whisrs toggle -l overrides it for one dictation. |
| `silence_timeout_ms` | integer (ms) | `2000` | 0 to 600000; `0` = off | Stop after this much silence once you spoke: streaming dictation, command mode, LLM commands and agent turns. Batch dictation stops on a second press. At 0 (off), every session waits for the second press. |
| `notify` | bool | `true` | `true`, `false` | Desktop notifications for dictation and errors. |
| `history` | bool | `true` | `true`, `false` | Keep finished dictations for whisrs log. Off stops new entries; whisrs log --clear removes the old ones. |
| `audio_feedback` | bool | `false` | `true`, `false` | Tones when dictation starts, stops and is done. |
| `audio_feedback_volume` | number | `0.5` | 0 to 1 | Volume of the tones. |
| `tray` | bool | `true` | `true`, `false` | Show the tray icon. It needs a StatusNotifierItem tray host, such as waybar's tray module, KDE Plasma, or GNOME with the AppIndicator extension. Needs the `tray` build feature. |
| `backend` | string | `groq` | `groq`, `deepgram-streaming`, `deepgram`, `openai-realtime`, `openai`, `openai-compatible-realtime`, `local-whisper`, `asr-sidecar`, `local-vosk` (coming soon), `local-parakeet` (coming soon) | The transcription backend. Its key or model is below. local-vosk and local-parakeet are not implemented yet; set their model_path in config.toml. |
| `vocabulary` | list | none | strings | Names and terms sent to the backend as a hint. With a vocabulary.txt, a changed list is written back there whole. |
| `prompt` | string | unset | text | Free-form context for the backend: style, register. Local whisper sends it with every phrase, so write it in the language you dictate in. |
| `remove_filler_words` | bool | `false` | `true`, `false` | Strip filler words (um, uh) from each dictation. |
| `filler_words` | list | built-in list | strings | Your own filler words. Empty uses the built-in list. |
| `overlay` | bool | `false` | `true`, `false` | Show a small pill on screen while you dictate or hear text read aloud. Recordings show none. Needs the `overlay` build feature. |
| `llm_post_process` | bool | `false` | `true`, `false` | Run every dictation through the LLM with llm_instruction before it is typed. Batch backends only. |
| `llm_instruction` | string | `Fix punctuation, capitalization and obvious transcription errors in the following text. Keep the wording and the meaning unchanged. Return only the corrected text, with no explanations and no quotes.` | text | What the LLM does with each dictation when post-processing is on. |

## [deepgram]

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `api_key` | API key | unset | your key | API key for the deepgram backends. WHISRS_DEEPGRAM_API_KEY wins over it. |
| `model` | string | `nova-3` | `nova-3`, `nova-3-medical`, `nova-3-pharma`, `nova-2`, `nova-2-meeting`, `nova-2-phonecall`, `nova-2-finance`, `nova-2-conversationalai`, `nova-2-voicemail`, `nova-2-video`, `nova-2-medical`, `nova-2-drivethru`, `nova-2-automotive`, `nova-2-atc`, or any other | The Deepgram model. Nova-3 models take your vocabulary as keyterms; older ones get none of it. |

## [groq]

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `api_key` | API key | unset | your key | API key for groq. WHISRS_GROQ_API_KEY wins over it. |
| `model` | string | `whisper-large-v3-turbo` | `whisper-large-v3-turbo`, `whisper-large-v3`, or any other | The Whisper model on Groq. |

## [openai]

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `api_key` | API key | unset | your key | API key for the openai backends. WHISRS_OPENAI_API_KEY wins over it. |
| `model` | string | `gpt-4o-mini-transcribe` | `gpt-transcribe`, `gpt-live-transcribe`, `gpt-realtime-whisper`, `gpt-4o-transcribe`, `gpt-4o-mini-transcribe`, `whisper-1`, or any other | The model of both openai backends. With no [openai] section, openai-realtime uses gpt-realtime-whisper. A recording on openai sends gpt-transcribe in place of a realtime-only one. |
| `languages` | list | none | language codes (`en`, `pt-BR`) | Expected input languages for gpt-live-transcribe or gpt-transcribe on openai-realtime. Overrides [general] language. |

## [local-whisper]

Needs the `local-whisper` build feature.

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `model_path` | path | unset (uses whisrs/models/ggml-base.en.bin in the data folder: `$XDG_DATA_HOME`, `~/.local/share` by default) | a `ggml-*.bin` file | The ggml model file, as a full path. config.toml does not expand ~; whisrs config expands it when it saves. whisrs setup downloads models into the models folder, and whisrs config lists those. |
| `segmentation` | string | `silence` | `silence`, `window`, `none` | When whisper types: after each pause, in a sliding window, or all at the end. |
| `phrase_silence_ms` | integer (ms) | `400` | 0 to 10000 | The silence that ends a phrase with silence segmentation. Shorter types sooner, in smaller pieces; longer keeps a phrase together and types it later. |
| `flash_attn` | bool | `false` | `true`, `false` | whisper.cpp flash attention. Only helps on some GPU builds. |

## [asr-sidecar]

Guide: [ASR sidecar servers](configuration.md#asr-sidecar-servers).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `url` | string | `http://127.0.0.1:8765/transcribe` | `http://127.0.0.1:8765/transcribe`, `https://openrouter.ai/api/v1/audio/transcriptions`, or another URL | The sidecar's transcription endpoint: a server of your own, or OpenRouter. |
| `model` | string | `microsoft/VibeVoice-ASR-HF` | OpenRouter's models (whisrs config lists them), or your server's model name | The model name sent to the sidecar. Listed for OpenRouter, with the price of an hour of audio; a server of your own names its own. Left at the default on OpenRouter, openai/whisper-large-v3-turbo is sent. |
| `api_key` | API key | unset | your key | The sidecar's bearer token: optional for a server of your own, OpenRouter needs it. WHISRS_ASR_SIDECAR_API_KEY wins over it. |

## [openai-compatible-realtime]

Guide: [Realtime servers](configuration.md#realtime-servers).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `url` | string | unset | text | WebSocket endpoint of the OpenAI-compatible realtime server. |
| `model` | string | `Whisper-Tiny` | text | The model the realtime server loads. |
| `profile` | string | `lemonade` | `lemonade`, `llamacpp`, `openai` | Which server dialect to speak. lemonade and llamacpp work here; openai does not. |
| `turn_detection` | string | `server-vad` | `server-vad`, `manual-commit` | Who decides where a turn ends. |
| `api_key` | API key | unset | your key | Optional bearer token for the realtime server. |

## [audio]

Guide: [Speech detection](configuration.md#speech-detection).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `device` | string | `default` | `default`, or a source from `pactl list short sources` | The microphone: default, or a source the sound server lists. A monitor source records what the speakers play. |
| `vad` | string | `earshot` | `earshot`, `rms` | What decides a sound is speech, for auto-stop, the skips and recordings: earshot, a small learned model that holds up in background noise, or rms, a plain loudness test. The Mic test's quality line uses it too. |

## [replacements]

Guide: [Word replacements](configuration.md#word-replacements).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `"phrase"` | string | none | the text it becomes | Whole-word fix-ups applied to every dictation and recording transcript: package build becomes PKGBUILD. |

## [input]

Guide: [Terminal detection](configuration.md#terminal-detection).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `backend` | string | `auto` | `auto`, `uinput`, `wayland-vk`, `xtest` | How text is typed. |
| `key_delay_ms` | integer (ms) | `2` | 0 to 1000 | The pause between keys. Raise it if a terminal app drops characters. |
| `modifier_wait_ms` | integer (ms) | `10000` | 0 to 600000; `0` = no wait | How long to wait for held Super, Alt, Ctrl or Shift before each key; if one is still held, the rest goes to the clipboard. At 0 (no wait), it goes there at once. |
| `paste` | bool | `false` | `true`, `false` | Paste with Ctrl+V instead of typing (batch backends, command mode and LLM commands). |
| `clipboard_fallback` | bool | `false` | `true`, `false` | Also leave the text in the clipboard, to paste if typing went wrong. |
| `clipboard_only` | bool | `false` | `true`, `false` | Only copy the text to the clipboard, never type it. |
| `terminal_classes` | list | none | strings | Window classes to treat as terminals, beside the built-in list. Matched whole, any case. |

## [hotkeys]

Guide: [Hotkeys](configuration.md#hotkeys).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `toggle` | hotkey | unset | a key combo ([syntax](configuration.md#hotkeys)) | Start and stop dictation. Most setups bind whisrs toggle in the compositor instead. |
| `cancel` | hotkey | unset | a key combo ([syntax](configuration.md#hotkeys)) | Cancel dictation and discard the audio. |
| `command` | hotkey | unset | a key combo ([syntax](configuration.md#hotkeys)) | Command mode: speak an instruction, the LLM rewrites the selection. |
| `speak` | hotkey | unset | a key combo ([syntax](configuration.md#hotkeys)) | Read the selection aloud ([tts]). |
| `recording` | hotkey | unset | a key combo ([syntax](configuration.md#hotkeys)) | Start and stop a recording of mic and system audio. Needs the `recording` build feature. |
| `next_profile` | hotkey | unset | a key combo ([syntax](configuration.md#hotkeys)) | Switch to the next config profile: base, then the profiles by name. |

## [overlay]

Guide: [Overlay](configuration.md#overlay).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `theme` | string | `carbon` | `carbon`, `ember`, `cyan`, `custom` | Colours the pill and whisrs config. custom takes the [overlay.colors] values, and ember's colours for those left unset. |
| `position` | string | `bottom-center` | `bottom-center`, `bottom-left`, `bottom-right`, `top-center`, `top-left`, `top-right` | Where the pill sits. |
| `width` | integer (px) | `100` | 90 to 120 | Pill width. |
| `height` | integer (px) | `40` | 36 to 48 | Pill height. |

## [overlay.colors]

Guide: [Overlay](configuration.md#overlay).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `recording` | colour | unset (the theme's) | `#RGB`, `#RRGGBB` or `#RRGGBBAA` | The custom theme's recording colour, and whisrs config's accent. |
| `transcribing` | colour | unset (the theme's) | `#RGB`, `#RRGGBB` or `#RRGGBBAA` | The custom theme's transcribing colour. |
| `speaking` | colour | unset (the theme's) | `#RGB`, `#RRGGBB` or `#RRGGBBAA` | The custom theme's read-aloud colour. |
| `background` | colour | unset (the theme's) | `#RGB`, `#RRGGBB` or `#RRGGBBAA` | The custom theme's pill background. |
| `ring` | colour | unset (the theme's) | `#RGB`, `#RRGGBB` or `#RRGGBBAA` | The custom theme's pill outline. |
| `glow` | colour | unset (the theme's) | `#RGB`, `#RRGGBB` or `#RRGGBBAA` | The custom theme's glow. |

## [agent_overlay]

Guide: [The agent overlay](agents.md#the-agent-overlay).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `enabled` | bool | unset (follows overlay) | `true`, `false` | The agent overlay: a row per working agent, in a corner. Only with [[agents]]. |
| `style` | string | `orb` | `orb`, `face` | How the agent overlay draws each agent. |
| `corner` | string | `bottom-right` | `bottom-right`, `bottom-left`, `top-right`, `top-left` | The agent overlay's corner. |

## [tts]

Guide: [Read aloud](configuration.md#read-aloud).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `enabled` | bool | `false` | `true`, `false` | Read the selection aloud with whisrs speak, and speak agent replies. |
| `backend` | string | `groq` | `groq`, `openai`, `deepgram`, `tts-sidecar` | Who reads the text: a cloud voice, or tts-sidecar, which asks a server of your own or OpenRouter. |
| `model` | string | unset (backend default) | the backend's models (whisrs config lists them) | The TTS model, from the backend's list (OpenRouter's, on its url). With deepgram the model is the voice. |
| `voice` | string | unset (backend default) | the model's voices (whisrs config lists them) | The voice, as the backend spells it. On OpenRouter each model has its own. deepgram does not use it: its voice is in the model. |
| `api_key` | API key | unset | your key | Unset, the backend's own key is used ([groq], [openai] or [deepgram]). tts-sidecar sends only this one, which OpenRouter needs. |
| `url` | string | unset | `http://127.0.0.1:8880/v1/audio/speech`, `https://openrouter.ai/api/v1/audio/speech`, or another URL | The tts-sidecar endpoint (/v1/audio/speech): a server of your own, or OpenRouter. |
| `response_format` | string | `wav` | `wav`, `pcm`, or any other | The audio format asked for: wav, or pcm (raw 16-bit audio, which whisrs plays too). OpenRouter refuses wav, so whisrs asks it for pcm. |

## [[agents]]

Guide: [Agent mode](agents.md).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `name` | string | required | text | Used by whisrs agent &lt;name>: a-z, 0-9, _ and -. |
| `hotkey` | hotkey | unset | a key combo ([syntax](configuration.md#hotkeys)) | Records a voice turn for this agent. |
| `new_session_hotkey` | hotkey | unset | a key combo ([syntax](configuration.md#hotkeys)) | Records a voice turn that starts a fresh session. |
| `harness` | string | `claude` | `claude`, `opencode`, `codex`, `copilot` | The coding agent this entry drives. |
| `dir` | path | required | a path (`~` is your home) | The directory the agent works in. It must exist. |
| `permissions` | string | `auto` | `auto`, `bypass`, `ask` | What the agent may do without asking. |
| `command` | string | unset (harness on PATH) | text | The harness binary. |
| `args` | list | none | strings | Extra arguments for the harness. |
| `reply_instruction` | string | unset (built-in) | text | Replaces the built-in voice instruction. |
| `feedback` | string | `brief` | `brief`, `summary`, `conversational` | How much the agent says while it works. |

## [llm]

Guide: [LLM commands](configuration.md#llm-commands).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `api_key` | API key | unset | your key | API key for the LLM provider: command mode, LLM commands, post-processing, recording summaries and speaker names. Unset, WHISRS_OPENAI_API_KEY is used, else WHISRS_GROQ_API_KEY. A local server that needs no key still needs a placeholder here. |
| `model` | string | `gpt-4o-mini` | the provider's chat models | The chat model. Suggested for the OpenAI, Groq, OpenRouter, Gemini and Ollama URLs; any other name can be typed. |
| `api_url` | string | `https://api.openai.com/v1/chat/completions` | `https://api.openai.com/v1/chat/completions`, `https://api.groq.com/openai/v1/chat/completions`, `https://openrouter.ai/api/v1/chat/completions`, `https://generativelanguage.googleapis.com/v1beta/openai/chat/completions`, `http://localhost:11434/v1/chat/completions`, or another URL | The chat completions endpoint. |

## [[llm_commands]]

Guide: [LLM commands](configuration.md#llm-commands).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `name` | string | required | text | Used by whisrs llm-command &lt;name>. |
| `hotkey` | hotkey | required | a key combo ([syntax](configuration.md#hotkeys)) | Dictate, and the LLM applies the instruction before typing. |
| `set_hotkey` | hotkey | unset | a key combo ([syntax](configuration.md#hotkeys)) | Press it with text selected to make the selection the new instruction. |
| `instruction` | string | required | text | What the LLM does with what you said. |

## [recording]

Needs the `recording` build feature. Guide: [Recording](recording.md).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `backend` | string | unset (same as dictation) | `groq`, `openai`, `deepgram`, `asr-sidecar`, `local-whisper` | The backend recordings are transcribed with, after you stop. It must take a file: the realtime ones cannot. |
| `user_name` | string | unset (your account's name) | text | Your name in transcripts. |
| `language` | string | unset ([general] language) | `auto`, or a language code (`en`, `pt-BR`) | The language of both tracks, or auto. |
| `mic_language` | string | unset (as language) | `auto`, or a language code (`en`, `pt-BR`) | The language of your microphone. |
| `system_language` | string | unset (as language) | `auto`, or a language code (`en`, `pt-BR`) | The language of the system audio. |
| `mic_device` | string | unset ([audio] device) | `default`, or a source from `pactl list short sources` | The microphone for recordings. |
| `system_device` | string | unset (default monitor) | `default`, `none` (mic only), or a source name ending in `.monitor` | What your computer plays: a monitor source, or none for the mic only. |
| `max_duration_minutes` | integer (minutes) | `180` | 1 to 2160 | Stop a recording after this long, at most 36 hours. |
| `audio_retention_days` | integer (days) | `7` | -1 to 3650; `-1` = keep forever; `0` = delete once transcribed | How long a recording's audio is kept. The transcript always stays. |
| `dir` | path | unset (~/.local/share/whisrs/recordings) | a path (`~` is your home) | Where recordings are kept. |
| `diarization` | string | `auto` | `auto`, `backend`, `sidecar`, `off` | How the voices on the system audio are told apart. |
| `diarization_url` | string | unset | a URL | A diarization sidecar, such as http://127.0.0.1:8770/diarize. |
| `num_speakers` | integer | `0` (auto) | 0 to 20; `0` = auto | How many voices the sidecar should find. At 0 (auto), the sidecar decides. |
| `shared_mic` | bool | `false` | `true`, `false` | Several people share your mic: tell its voices apart too. |
| `name_speakers` | bool | `false` | `true`, `false` | Name the speakers with the [llm]. Sends excerpts of each transcript to it. |
| `summary` | bool | `false` | `true`, `false` | Summarize each transcript with the [llm]. Sends the whole transcript to it. |
| `summary_instruction` | string | unset (default rules) | text | What a summary should look like, in place of the default rules. |

## [hooks]

Needs the `hooks` build feature. Guide: [Hooks](configuration.md#hooks).

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `media_auto_pause` | bool | `false` | `true`, `false` | Pause the media that is playing while whisrs listens to you, and resume exactly that after. Recordings pause nothing. |
| `on_record_start` | string | unset | text | A shell command run when whisrs starts listening to you: dictation, command mode, LLM commands and agent voice turns. Recordings run none. |
| `on_record_stop` | string | unset | text | A shell command run when whisrs stops listening to you. Recordings run none. |

## [local-vosk]

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `model_path` | path | unset | a path | The backend this configures is not implemented yet. |

## [local-parakeet]

| Key | Type | Default | Values | Description |
|---|---|---|---|---|
| `model_path` | path | unset | a path | The backend this configures is not implemented yet. |
