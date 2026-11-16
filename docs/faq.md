# FAQ

These answers add to the [FAQ in the README](../README.md#faq).

## Which speech recognition backends does whisrs support?

whisrs has eight working backends: `groq`, `deepgram`, `deepgram-streaming`, `openai`, `openai-realtime`, `openai-compatible-realtime`, `local-whisper` and `asr-sidecar`, which reaches any OpenAI-compatible transcription server, local or hosted, including [OpenRouter](configuration.md#openrouter). Which of them type while you speak is in [Batch and streaming backends](configuration.md#batch-and-streaming-backends). `local-vosk` and `local-parakeet` are placeholders for native backends not built yet (Parakeet already runs through `asr-sidecar`).

## How does whisrs type text into applications?

whisrs types keystrokes through a virtual keyboard that follows your layout, so the text lands in any app, terminals included; [Typing](configuration.md#typing) covers the backends and paste.

## Can whisrs read text aloud?

Yes: with `[tts] enabled = true`, `whisrs speak` (alias `read`) reads the selected text aloud through Groq, Deepgram, or a local or OpenRouter speech server ([Read aloud](configuration.md#read-aloud)).

## Which Linux distributions does whisrs support?

whisrs runs on any Linux distribution with ALSA and libxkbcommon, on x86_64 and aarch64, under systemd or OpenRC. Install it with the install script, a prebuilt tarball, the AUR, `cargo install whisrs --locked` or the Nix flake ([Installation](../README.md#installation)). Building from source also needs clang and cmake for the bundled whisper.cpp.
