# OpenAI-Compatible Realtime ASR Tasks

Implementation task plan for `specs/openai-compatible-realtime/README.md` and `DESIGN.md`.

## Notes

- Repo instructions are in `CLAUDE.md`; no `AGENTS.md` exists.
- This is a Rust backend/client integration, not a new whisrs-managed Python sidecar.
- Keep the existing batch HTTP `asr-sidecar` backend unchanged.
- Keep the public OpenAI cloud backend name and config unchanged: `backend = "openai-realtime"` with `[openai]`.
- Add the external backend as `backend = "openai-compatible-realtime"` with `[openai-compatible-realtime]`.
- The current daemon streaming channel is append-only `String`; Lemonade replaceable partials must not be typed in the first version.
- Lemonade partials should still be received and may update an internal latest-interim buffer. In v1, only `completed` Lemonade transcripts are emitted to the daemon, so realtime behavior is phrase/utterance-level rather than token-level replacement.

## Phase 1: Shared Protocol Module

- [ ] Create `src/transcription/openai_realtime_protocol.rs`.
- [ ] Move OpenAI-compatible client message types out of `src/transcription/openai_realtime.rs`:
  - [ ] `session.update`
  - [ ] `input_audio_buffer.append`
  - [ ] `input_audio_buffer.commit`
- [ ] Move OpenAI-compatible server event parsing out of `src/transcription/openai_realtime.rs`:
  - [ ] `conversation.item.input_audio_transcription.delta`
  - [ ] `conversation.item.input_audio_transcription.completed`
  - [ ] `conversation.item.input_audio_transcription.failed`
  - [ ] `error`
  - [ ] session/audio-buffer lifecycle events used for logging.
- [ ] Move PCM16 little-endian base64 encoding into the shared module.
- [ ] Move the existing 16 kHz to 24 kHz resampler into the shared module.
- [ ] Keep the existing OpenAI prompt clamp behavior and 1024 character limit.
- [ ] Add `OpenAiRealtimeProfile` with `OpenAi` and `Lemonade`.
- [ ] Add `TurnDetectionMode` with `ServerVad` and `ManualCommit`.
- [ ] Add profile helpers for parsing supported profile names, input sample rate, delta semantics, session update serialization, and commit behavior.
- [ ] Add `OpenAiRealtimeProtocolEngine` with `new`, `transcribe`, and `transcribe_stream`.
- [ ] Implement one shared WebSocket connect/send/receive lifecycle in the engine.
- [ ] Ensure provider wrappers do not serialize audio, parse server events, or own their own WebSocket loops.

## Phase 2: Correct Stream Lifecycle

- [ ] Replace the current “first completed event is terminal” behavior with commit-aware finalization.
- [ ] Track whether the input audio channel has closed.
- [ ] Track whether `input_audio_buffer.commit` has been sent.
- [ ] Track latest interim transcript text per item when replaceable deltas include an item ID.
- [ ] Allow any number of `completed` events before end-of-audio.
- [ ] Emit completed transcripts during recording when they represent stable utterances.
- [ ] Treat only post-commit completion as satisfying final flush.
- [ ] Add a final-response timeout with clear error context.
- [ ] Close the WebSocket cleanly after final completion, explicit server close, error, or timeout.
- [ ] Preserve text already sent to the daemon if a later stream error occurs.

## Phase 3: Transcript Semantics

- [ ] For `OpenAiRealtimeProfile::OpenAi`, continue emitting non-empty append-only deltas to `text_tx`.
- [ ] For `OpenAiRealtimeProfile::Lemonade`, parse interim `delta` events without sending them to `text_tx`.
- [ ] For Lemonade deltas, update latest-interim state when an item ID is available.
- [ ] For Lemonade deltas without an item ID, debug-log the interim text but do not emit it.
- [ ] For Lemonade, emit each non-empty `completed` transcript once.
- [ ] Clear latest-interim state for an item after its `completed` transcript is handled.
- [ ] Suppress duplicate completed transcripts, preferring stable item IDs if present and otherwise using trimmed transcript text.
- [ ] Avoid re-emitting completed text already emitted through append-only deltas.
- [ ] Do not change the daemon-level `TranscriptionBackend::transcribe_stream` signature in this feature.
- [ ] Document in code comments that Lemonade v1 typing is completed-utterance realtime, not live interim replacement.

## Phase 4: Refactor Existing OpenAI Backend

- [ ] Update `src/transcription/mod.rs` to export the shared protocol module.
- [ ] Refactor `src/transcription/openai_realtime.rs` into a thin wrapper.
- [ ] Preserve `OpenAIRealtimeBackend::new(api_key: String) -> Self`.
- [ ] Keep API key resolution from configured `[openai] api_key` and `WHISRS_OPENAI_API_KEY`.
- [ ] Configure the wrapper with the fixed OpenAI realtime URL, bearer auth, and `OpenAiRealtimeProfile::OpenAi`.
- [ ] Delegate both `transcribe()` and `transcribe_stream()` to `OpenAiRealtimeProtocolEngine`.
- [ ] Keep `supports_streaming() -> true`.
- [ ] Confirm existing OpenAI behavior remains covered by tests.

## Phase 5: Add External Backend Wrapper

- [ ] Create `src/transcription/openai_compatible_realtime.rs`.
- [ ] Add `OpenAiCompatibleRealtimeBackend`.
- [ ] Add constructor validation for non-empty URL, `ws`/`wss` scheme, non-empty model, supported profile, and supported turn detection.
- [ ] Support optional bearer auth from `api_key`.
- [ ] Configure the shared engine with `OpenAiRealtimeProfile::Lemonade`.
- [ ] Delegate both `transcribe()` and `transcribe_stream()` to the shared engine.
- [ ] Return `supports_streaming() -> true`.
- [ ] Ensure logs use sanitized endpoint display and never include bearer tokens.

## Phase 6: Config Model and Validation

- [ ] Add `OpenAiCompatibleRealtimeConfig` to `src/lib.rs`.
- [ ] Add `Config.openai_compatible_realtime` with `#[serde(default, rename = "openai-compatible-realtime")]`.
- [ ] Add defaults for model, profile, and turn detection.
- [ ] Update `Config::validate()` to accept `openai-compatible-realtime`.
- [ ] Reject missing/empty URL, non-WebSocket URL, empty model, unknown profile, and unknown turn detection.
- [ ] Update unknown-backend error text to include `openai-compatible-realtime`.
- [ ] Update `Config::has_any_backend_configured()` to treat a configured external realtime URL as configured.
- [ ] Add config parse/validation tests in `src/lib.rs`.

## Phase 7: Daemon Wiring

- [ ] Import `OpenAiCompatibleRealtimeBackend` in `src/daemon/main.rs`.
- [ ] Add `openai-compatible-realtime` case in `create_backend()`.
- [ ] Avoid logging full configured URL if it may contain credentials or sensitive query parameters.
- [ ] Add `openai-compatible-realtime` case in `get_model_for_backend()`.
- [ ] Verify `build_transcription_config()` needs no API changes.
- [ ] Confirm `run_streaming_pipeline()` consumes only completed Lemonade transcript chunks as normal append-only strings.
- [ ] Confirm no Lemonade interim delta is sent to the daemon text channel.
- [ ] Keep batch HTTP `asr-sidecar` daemon wiring unchanged.

## Phase 8: Setup and Config Editor

- [ ] Update `src/config/setup.rs` backend choices to include `OpenAI-compatible Realtime`.
- [ ] Add backend value `openai-compatible-realtime`.
- [ ] Update default selection mapping for existing configs.
- [ ] Add prompts for WebSocket URL, model, profile, turn detection, and optional API key.
- [ ] Replace the growing `configure_backend()` return tuple with a small struct, or extend the tuple carefully if keeping the existing pattern.
- [ ] Update `run_setup()` to write the new config section.
- [ ] Update `src/config/edit.rs` to preserve and update `[openai-compatible-realtime]`.
- [ ] Update current key summary to treat this as an optional-key external backend.
- [ ] Ensure save validation errors are user-readable.

## Phase 9: Documentation

- [ ] Update `README.md` backend table to include `OpenAI-compatible Realtime`.
- [ ] Update README minimal config backend list.
- [ ] Update `docs/configuration.md` backend list.
- [ ] Add `[openai-compatible-realtime]` config reference to `docs/configuration.md`.
- [ ] Update `contrib/asr-sidecars/README.md` to distinguish batch HTTP sidecars from OpenAI-compatible realtime external servers.
- [ ] Update `specs/openai-compatible-realtime/README.md` references now that the spec lives under `specs/`.
- [ ] Link `DESIGN.md` and `TASKS.md` from the spec README if useful.

## Phase 10: Unit Tests

- [ ] Add or move protocol tests into `src/transcription/openai_realtime_protocol.rs`.
- [ ] Test OpenAI session update serialization, including nested shape, 24 kHz format, model, auto language omission, prompt handling, and manual-commit behavior.
- [ ] Test Lemonade session update serialization, including flat `session.model`, server VAD, manual commit, and omitted undocumented prompt/language fields.
- [ ] Test audio handling: Lemonade direct 16 kHz, OpenAI 24 kHz resampling, and base64 PCM round trip.
- [ ] Test append, commit, delta, completed, failed, and error message serialization/parsing.
- [ ] Test event semantics for OpenAI deltas, Lemonade buffered/logged deltas, completed emission, duplicate suppression, pre-close completion, and post-commit finalization.
- [ ] Test Lemonade partial sequence such as `hel`, `hello wor`, `hello world` emits nothing until `completed`.
- [ ] Test Lemonade `completed = "hello world"` emits exactly one `"hello world"` after that partial sequence.
- [ ] Add wrapper tests in `src/transcription/openai_compatible_realtime.rs` for URL/profile/turn-detection validation.
- [ ] Add config tests in `src/lib.rs` for TOML parse, defaults, validation, and unknown-backend message.

## Phase 11: Mock WebSocket Integration Tests

- [ ] Add a local mock WebSocket test if it can remain stable without external services.
- [ ] Start a localhost WebSocket listener with `tokio_tungstenite`.
- [ ] Instantiate `OpenAiCompatibleRealtimeBackend` against the mock URL.
- [ ] Assert first message is Lemonade-style `session.update`.
- [ ] Send audio chunks and assert `input_audio_buffer.append` messages arrive.
- [ ] Send two completed events while the audio channel is still open and assert both are emitted.
- [ ] Before each completed event, send replaceable interim deltas and assert they are not emitted to `text_tx`.
- [ ] Close the audio channel and assert `input_audio_buffer.commit` arrives.
- [ ] Send final completed event and assert clean shutdown.
- [ ] Repeat with server `error` event.
- [ ] Repeat with final-response timeout using a short test-only timeout.

## Phase 12: Manual Acceptance

- [ ] Start Lemonade and resolve its realtime WebSocket URL.
- [ ] Configure `backend = "openai-compatible-realtime"` with a Lemonade `ws://` URL.
- [ ] Run with debug logging.
- [ ] Dictate multiple phrases separated by silence without manually stopping.
- [ ] Verify each completed phrase is typed once while recording remains active.
- [ ] Verify interim Lemonade partials do not appear at the cursor before completion.
- [ ] Stop during speech and verify trailing speech flushes after commit.
- [ ] Verify logs show Lemonade audio is encoded at 16 kHz without 24 kHz resampling.
- [ ] Verify no replaceable partial text is blindly appended.
- [ ] Note expected latency: Lemonade text appears after server-VAD completion or stop/commit, not on every interim hypothesis.
- [ ] Switch back to `backend = "openai-realtime"` and verify existing OpenAI behavior still works.

## Phase 13: Final Verification

- [ ] Run `cargo fmt`.
- [ ] Run `cargo fmt -- --check`.
- [ ] Run `cargo test`.
- [ ] Run `cargo clippy --all-targets -- -D warnings`.
- [ ] Run `cargo build`.
- [ ] Confirm `Cargo.toml` and `Cargo.lock` are unchanged unless a dependency was intentionally added.
- [ ] Confirm no bearer tokens or credential-bearing URLs appear in logs, tests, docs, or committed fixtures.
- [ ] Confirm `git diff` does not include unrelated formatting or refactors.

## Completion Checklist

- [ ] Lemonade realtime transcription works through `backend = "openai-compatible-realtime"`.
- [ ] Existing `openai-realtime` backend still works without config changes.
- [ ] OpenAI cloud and external realtime share one protocol engine and one message type set.
- [ ] There is no Lemonade-specific WebSocket loop outside the shared protocol engine.
- [ ] Existing batch HTTP `asr-sidecar` behavior is unchanged.
- [ ] Server-VAD completed items do not prematurely close active streams.
- [ ] End-of-audio commit flushes trailing audio.
- [ ] Replaceable Lemonade partials cannot corrupt typed text.
- [ ] Lemonade partials are received/buffered or logged internally, but only completed transcripts are emitted to the append-only daemon channel.
- [ ] Tests cover protocol serialization, event semantics, config validation, and at least one mock WebSocket happy path.
- [ ] Documentation clearly distinguishes HTTP sidecars from OpenAI-compatible realtime external servers.
