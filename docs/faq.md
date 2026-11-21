# FAQ

These answers add to the [FAQ in the README](../README.md#faq).

## Which speech recognition backends does whisrs support?

whisrs supports eight backends: `groq`, `deepgram`, `deepgram-streaming`, `openai`, `openai-realtime`, `openai-compatible-realtime`, `local-whisper` and `asr-sidecar`. `asr-sidecar` reaches any OpenAI-compatible transcription server, your own (Parakeet, Nemotron, Moonshine) or hosted, such as [OpenRouter](configuration.md#openrouter). [Batch and streaming backends](configuration.md#batch-and-streaming-backends) says which of them type while you speak.

## How does whisrs type text into applications?

whisrs types keystrokes through a virtual keyboard that follows your layout, so the text lands in any app, terminals included. [Typing](configuration.md#typing) covers the backends and paste.

## Can whisrs read text aloud?

whisrs reads the selected text aloud with `whisrs speak` (alias `read`) once `[tts] enabled = true`, through Groq, Deepgram, or a local or OpenRouter speech server ([Read aloud](configuration.md#read-aloud)).

## Which Linux distributions does whisrs support?

whisrs runs on any Linux distribution with ALSA and libxkbcommon, on x86_64 and aarch64, under systemd or OpenRC. Install it with the install script, a prebuilt tarball, the AUR, `cargo install whisrs --locked` or the Nix flake ([Installation](../README.md#installation)). The prebuilt full builds need AVX2 on x86_64 and ARMv8.2 with dotprod on aarch64 (Raspberry Pi 5 and Apple M1, not the Pi 4); on an older CPU, use a `-minimal` build (the install script picks it) or build from source. Building from source also needs clang and cmake for the bundled whisper.cpp.
