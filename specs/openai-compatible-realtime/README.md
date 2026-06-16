# OpenAI-compatible realtime ASR sidecar specification

Status: proposal for [issue #49](https://github.com/y0sif/whisrs/issues/49)

## Decision

Add a generic `openai-compatible-realtime` transcription backend for external
WebSocket services that implement the OpenAI Realtime transcription event
model. Lemonade is the first required compatibility target, but its name must
not become the whisrs backend name or the boundary of the Rust implementation.

This is a client/backend integration, not a new Python sidecar process shipped
by whisrs. The spec lives under `specs/openai-compatible-realtime/` because it
defines an external ASR server integration contract rather than a shipped
sidecar implementation.

The implementation must refactor and reuse the existing
`src/transcription/openai_realtime.rs` protocol code. It must not introduce a
second WebSocket send/receive loop or duplicate the OpenAI-compatible message
types.

## Issue review

Issue #49 is directionally sound: Lemonade's realtime endpoint closely matches
the event model whisrs already uses for OpenAI Realtime, and external realtime
ASR belongs in the model-agnostic sidecar story.

The implementation should adjust three parts of the issue's initial proposal:

- Use a protocol-oriented backend name instead of a Lemonade-specific name.
- Do not merely add a WebSocket mode to the current batch `AsrSidecarBackend`;
  share the existing OpenAI Realtime protocol engine through a separate,
  explicitly streaming backend.
- Do not blindly type Lemonade `delta` events. Lemonade documents them as
  replaceable partials, while the current whisrs typing pipeline only supports
  append-only strings. In the first version, Lemonade partials may be kept as
  in-memory interim state for logging/debugging, but only `completed`
  transcripts are emitted to the daemon.

The existing OpenAI Realtime lifecycle also needs correction during the
refactor: a `completed` event finishes one transcription item, not necessarily
the whole recording stream.

## Motivation

The current `asr-sidecar` backend sends one completed WAV recording to an HTTP
endpoint and waits for one final response. An OpenAI-compatible realtime
backend lets a user-managed local server receive microphone chunks during
recording and return transcript events before recording stops.

Lemonade is a useful first target because it exposes `WS /realtime`, accepts
`input_audio_buffer.append` and `input_audio_buffer.commit`, and emits
`conversation.item.input_audio_transcription.delta` and
`conversation.item.input_audio_transcription.completed`.

## Goals

- Support Lemonade's realtime transcription WebSocket as the first external
  OpenAI-compatible server.
- Keep the public name and shared implementation provider-neutral.
- Reuse one OpenAI-compatible realtime protocol engine for OpenAI cloud and
  external servers.
- Preserve the existing `openai-realtime` backend and configuration.
- Stream 16 kHz mono PCM16 directly to Lemonade without unnecessary
  resampling.
- Support server VAD and an explicit commit when whisrs stops recording.
- Never type duplicated or stale replaceable partial transcripts.

## Non-goals

- Replacing the existing batch HTTP `asr-sidecar` contract.
- Building or managing Lemonade from whisrs.
- Defining a universal WebSocket protocol for non-OpenAI-compatible ASR
  servers.
- Making every wire-level protocol field user-configurable.
- Typing replaceable partial hypotheses before whisrs has a safe text
  replacement mechanism.

## Compatibility differences

"OpenAI-compatible" does not mean the two endpoints are identical. The shared
engine must use explicit protocol profiles rather than hardcoding either
provider's behavior.

| Behavior | OpenAI cloud | Lemonade |
|---|---|---|
| URL | Fixed OpenAI URL with `intent=transcription` | User URL, commonly `ws://localhost:<port>/realtime?model=Whisper-Tiny` |
| Authentication | Required bearer token | None by default |
| Input audio | 24 kHz mono PCM16 | 16 kHz mono PCM16 |
| Session shape | Nested transcription session/audio input config | Flat `session.model` and `session.turn_detection` fields |
| Prompt/language | Supported by the current whisrs integration | Not documented for realtime |
| Delta semantics | Treated as append-only by whisrs | Documented as replaceable interim text |
| Completed event | Completes one transcription item | Completes one VAD/commit transcription item |

The capture pipeline already produces 16 kHz mono `i16` chunks. The Lemonade
profile must encode those chunks directly; the OpenAI profile must continue to
resample them to 24 kHz.

## Proposed configuration

Use a distinct backend name because transport and streaming behavior are
materially different from the batch HTTP `asr-sidecar` backend:

```toml
[general]
backend = "openai-compatible-realtime"
language = "auto"

[openai-compatible-realtime]
url = "ws://localhost:12345/realtime?model=Whisper-Tiny"
model = "Whisper-Tiny"
profile = "lemonade"
turn_detection = "server-vad"
```

Initial fields:

- `url`: required `ws://` or `wss://` endpoint.
- `model`: model identifier sent in `session.update`.
- `profile`: compatibility profile; initially only `lemonade`.
- `turn_detection`: `server-vad` by default, or `manual-commit`.
- `api_key`: optional bearer token for external servers that require one.

Do not accept arbitrary headers in the first version. Add a narrowly scoped
authentication option when a real compatible server requires it.

The Lemonade WebSocket port is dynamically assigned and discoverable from its
`/v1/health` response. Automatic discovery is a possible follow-up; the first
version requires the resolved WebSocket URL in config.

## Required refactor

Extract the reusable protocol and connection lifecycle from
`src/transcription/openai_realtime.rs` into a shared module, for example
`src/transcription/openai_realtime_protocol.rs`.

The shared module owns:

- OpenAI-compatible client and server message types.
- PCM little-endian/base64 encoding.
- Optional resampling from the whisrs capture rate to the profile rate.
- WebSocket connection, send loop, receive loop, timeouts, commit, and close.
- Session update serialization selected by protocol profile.
- Event parsing and error normalization.
- The non-streaming `transcribe()` bridge that feeds one WAV through the
  streaming engine.

Use a small enum with profile-specific methods, not two backend
implementations and not a speculative plugin trait:

```rust
enum OpenAiRealtimeProfile {
    OpenAi,
    Lemonade,
}
```

Profile methods should provide the input sample rate, session update payload,
supported request fields, delta semantics, and default VAD configuration.

Keep provider/configuration wrappers thin:

- `OpenAIRealtimeBackend` resolves the OpenAI API key, supplies the fixed
  OpenAI endpoint, and selects `OpenAiRealtimeProfile::OpenAi`.
- `OpenAiCompatibleRealtimeBackend` validates the configured URL and optional
  bearer token, then selects the configured external profile.

The wrappers delegate both `transcribe()` and `transcribe_stream()` to the
same shared engine. No wrapper should serialize audio messages, parse server
events, or own a WebSocket loop.

## Stream lifecycle

The shared engine must distinguish an item completion from a stream
completion. A `conversation.item.input_audio_transcription.completed` event
can occur multiple times while server VAD is active and must not close the
connection.

Required lifecycle:

1. Connect and send the profile-specific `session.update`.
2. Forward audio chunks until the whisrs audio channel closes.
3. Allow any number of VAD-triggered completed items during recording.
4. On end-of-audio, send `input_audio_buffer.commit` when required to flush
   trailing audio.
5. Wait for the post-commit item completion or an explicit terminal/error
   condition.
6. Close the WebSocket and finish the backend task.

The existing OpenAI implementation's terminal-event signaling must be moved
into this shared lifecycle and made commit-aware. A completed item observed
before end-of-audio must not satisfy the final post-commit wait.

## Transcript event semantics

The current `TranscriptionBackend::transcribe_stream` channel carries only
`String`, and the daemon assumes each string is append-only text. That is safe
for the current OpenAI delta handling but unsafe for Lemonade, whose interim
deltas are documented as replaceable.

This means Lemonade is realtime at the phrase/utterance level in the first
version, not at the token-by-token typing level. The backend still receives
interim updates as the user speaks, but those updates are not typed because
whisrs currently has no operation for "replace the text I just typed for this
same transcription item." If the backend typed every interim hypothesis, a
sequence such as `hel`, `hello wor`, `hello world` would appear to the user as
duplicated appended text.

For the first version:

- OpenAI append-only deltas continue to be emitted as they are today.
- Lemonade interim/replaceable deltas are parsed and may update a per-item
  `latest_interim` buffer for debugging/future UI, but they are not sent to the
  typing pipeline.
- Each non-empty Lemonade `completed` transcript is sent once to the typing
  pipeline. With server VAD, completed utterances can still be typed while the
  user continues recording.
- A completed transcript must not be emitted again if its text was already
  emitted through append-only deltas.

This is correctness-first realtime behavior: it streams completed VAD turns,
but does not type unstable partial hypotheses. It is still more responsive than
the batch HTTP `asr-sidecar` backend because text can appear after each
server-VAD-completed phrase instead of only after the entire recording stops.
It is less immediate than OpenAI append-only deltas, where text can appear
while an utterance is still in progress.

A follow-up can replace the `String` channel with a normalized event type such
as `Append`, `ReplaceInterim`, and `Finalize`. That requires a daemon-level
replacement strategy before `ReplaceInterim` may be typed. Blindly appending
replaceable partials is not acceptable.

## Implementation areas

Expected code changes:

- Add the shared OpenAI-compatible realtime protocol module.
- Refactor `OpenAIRealtimeBackend` into a thin wrapper over the shared engine.
- Add `OpenAiCompatibleRealtimeBackend`.
- Add `OpenAiCompatibleRealtimeConfig` to the top-level config.
- Add backend creation, model selection, validation, setup/editor, and
  documentation entries for `openai-compatible-realtime`.
- Keep the existing batch `AsrSidecarBackend` unchanged.

## Validation and errors

- Reject empty URLs and URLs whose scheme is not `ws` or `wss`.
- Reject unknown profiles with a message listing supported profiles.
- Reject empty audio in non-streaming `transcribe()`.
- Surface WebSocket handshake errors, server `error` events, malformed session
  updates, and final-response timeouts with the external endpoint in context.
- Do not log bearer tokens or full configured URLs when they may contain
  credentials.
- Preserve any text already typed if the stream later fails.

## Test plan

Unit tests:

- OpenAI profile serializes the existing nested 24 kHz session update.
- Lemonade profile serializes flat `session.model` and VAD/manual-commit
  variants.
- Lemonade audio remains 16 kHz; OpenAI audio is resampled to 24 kHz.
- Append, commit, delta, completed, and error messages parse correctly.
- Replaceable Lemonade deltas are not emitted to the typing channel.
- Replaceable Lemonade deltas can update an internal latest-interim buffer, but
  only completed transcripts are emitted.
- Multiple completed items before end-of-audio do not terminate the stream.
- Only a post-commit completion satisfies the final flush wait.
- Existing OpenAI prompt, language, and manual-commit behavior remains
  covered.

Integration test with a local mock WebSocket server:

1. Assert the expected session update is received.
2. Assert audio append messages arrive before the input channel closes.
3. Send two VAD completed items while recording and verify both are emitted.
4. Receive the final commit, send one final completed item, and verify clean
   shutdown.
5. Repeat with an error event and a timeout.

Manual Lemonade acceptance:

- Start Lemonade and resolve its realtime WebSocket port.
- Dictate multiple phrases separated by silence without stopping whisrs.
- Verify each completed phrase is typed once while recording remains active.
- Stop during speech and verify the trailing phrase is flushed by commit.
- Verify no duplicated text and no 16 kHz to 24 kHz resampling.
- Verify interim Lemonade deltas do not appear at the cursor until the matching
  completed transcript arrives.

## Acceptance criteria

- Lemonade realtime transcription works through
  `backend = "openai-compatible-realtime"`.
- The existing OpenAI Realtime backend still passes its tests and works
  without configuration changes.
- OpenAI and external realtime backends use one shared WebSocket protocol
  engine and one set of message types.
- The implementation contains no Lemonade-specific WebSocket loop.
- Server-VAD item completions do not prematurely close an active stream.
- Replaceable partials cannot corrupt typed text.
- Lemonade realtime behavior is documented as phrase/utterance-level typing in
  the first version, not token-level partial replacement.

## References

- [whisrs issue #49](https://github.com/y0sif/whisrs/issues/49)
- [Lemonade OpenAI-compatible API: `WS /realtime`](https://lemonade-server.ai/docs/api/openai/#ws-realtime)
- Existing implementation: `src/transcription/openai_realtime.rs`
- Existing batch sidecar contract: `contrib/asr-sidecars/README.md`
