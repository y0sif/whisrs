# OpenAI-Compatible Realtime ASR Design

## 1. Summary

Build a new `openai-compatible-realtime` transcription backend for external WebSocket ASR services that follow the OpenAI Realtime transcription event model. Lemonade is the first target, but the Rust backend and public config name stay provider-neutral.

Main behavior:

- Users configure `backend = "openai-compatible-realtime"` and a `ws://` or `wss://` realtime endpoint.
- whisrs streams existing microphone `AudioChunk` values to that endpoint during recording.
- The backend sends OpenAI-compatible `input_audio_buffer.append` and `input_audio_buffer.commit` messages.
- The backend parses `conversation.item.input_audio_transcription.*` events.
- Lemonade completed transcripts are typed once; replaceable interim deltas are parsed and may update an internal latest-interim buffer, but are not sent to the current append-only typing pipeline.
- The existing `openai-realtime` backend keeps its config shape and behavior, but delegates protocol details to the same shared engine.

Important constraints:

- Do not modify or replace the existing batch HTTP `asr-sidecar` backend.
- Do not create a Lemonade-specific backend or duplicate WebSocket loops.
- Preserve the current `TranscriptionBackend` trait contract: streaming backends receive `mpsc::Receiver<AudioChunk>` and emit append-only `String` chunks.
- Fix the current OpenAI realtime lifecycle bug where a `completed` item is treated as terminal even though server VAD can produce multiple completed items during one recording.
- Do not log bearer tokens or credential-bearing URLs.
- For Lemonade, first-version "realtime" means phrase/utterance-level typing on `completed` events, not live replacement of interim hypotheses at the cursor.

## 2. Repo Context

Repo instructions are in `CLAUDE.md`; there is no `AGENTS.md`. Relevant conventions:

- Rust workspace, main package has binaries `whisrs` and `whisrsd`.
- Use `anyhow` in application/backend flows and `thiserror` for shared error types.
- Use `tracing` for logging.
- Config structs derive `Serialize` and `Deserialize`.
- Config file is TOML at `~/.config/whisrs/config.toml`, written with `0600` permissions.
- IPC is length-prefixed JSON, but this feature does not need IPC changes.

Relevant files and APIs:

- `src/transcription/mod.rs`
  - Defines `TranscriptionBackend`.
  - Default `transcribe_stream()` collects audio into WAV, so true streaming backends override it and return `supports_streaming() == true`.
- `src/transcription/openai_realtime.rs`
  - Existing OpenAI Realtime WebSocket implementation.
  - Contains message structs, 16 kHz to 24 kHz resampling, base64 PCM encoding, send loop, receive loop, and unit tests.
  - This is the code to refactor into a reusable protocol module.
- `src/transcription/asr_sidecar.rs`
  - Existing batch HTTP sidecar backend. It should remain unchanged except for docs that distinguish it from the realtime backend.
- `src/lib.rs`
  - Defines `Config`, backend config structs, defaults, validation, and tests.
  - Add the new config section here.
- `src/daemon/main.rs`
  - `create_backend()` instantiates backend implementations.
  - `get_model_for_backend()` selects model strings passed through `TranscriptionConfig`.
  - `run_streaming_pipeline()` already supports streaming text as append-only `String` chunks and saves history.
- `src/config/setup.rs`
  - Interactive `whisrs setup` backend selection and backend-specific prompts.
- `src/config/edit.rs`
  - Interactive `whisrs config` backend editing and current backend summary.
- `docs/configuration.md` and `README.md`
  - Backend and config documentation to update.

Existing dependencies already cover the implementation:

- `tokio`, `tokio-tungstenite`, `futures-util` for WebSocket streaming.
- `serde`, `serde_json`, `toml` for wire messages and config.
- `base64` and `hound` for audio encoding/non-streaming bridge.
- No new dependency is required for the first version.

Build/test/lint commands from `CLAUDE.md`:

```bash
cargo fmt
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

## 3. Assumptions

- The whisrs capture pipeline continues to produce 16 kHz mono `i16` chunks. Lemonade can consume those chunks directly; OpenAI cloud still needs 24 kHz PCM.
- Lemonade's configured URL includes any dynamic port and model query string discovered externally. Automatic `/v1/health` discovery is a follow-up.
- `turn_detection = "server-vad"` means send VAD config in `session.update` and still commit on end-of-audio to flush trailing speech.
- `turn_detection = "manual-commit"` means omit server VAD config and only complete after explicit commit.
- External auth only supports an optional bearer token in `api_key`. Arbitrary user-specified headers are out of scope.
- The current daemon typing path cannot replace already typed text, so replaceable interim events must not be emitted.
- Lemonade interim deltas are still useful to receive: the protocol engine can keep the latest interim text per transcription item for debug logging and future UI/replacement support, while withholding it from `text_tx`.

## 4. Architecture

Main components:

- `OpenAiRealtimeProtocolEngine`
  - Shared async implementation for OpenAI-compatible realtime transcription.
  - Owns WebSocket connection setup, session update serialization, audio send loop, receive loop, commit/finalization, and non-streaming bridge.
- `OpenAiRealtimeProfile`
  - Small enum describing wire differences:
    - `OpenAi`
    - `Lemonade`
  - Provides sample rate, session update shape, delta behavior, default turn detection, and prompt/language support.
- `OpenAIRealtimeBackend`
  - Thin wrapper for the existing cloud backend.
  - Resolves `WHISRS_OPENAI_API_KEY` or `[openai] api_key`.
  - Uses fixed URL `wss://api.openai.com/v1/realtime?intent=transcription`.
  - Delegates `transcribe()` and `transcribe_stream()` to the shared engine with `OpenAiRealtimeProfile::OpenAi`.
- `OpenAiCompatibleRealtimeBackend`
  - New external backend.
  - Validates configured URL/profile/auth.
  - Delegates to the shared engine with `OpenAiRealtimeProfile::Lemonade`.

Control flow:

1. User starts recording through existing CLI/daemon IPC.
2. `src/daemon/main.rs` creates a `TranscriptionConfig` with language, model, and prompt.
3. Because `supports_streaming()` is true, `run_streaming_pipeline()` starts immediately.
4. The backend wrapper constructs a protocol engine request and calls shared `transcribe_stream()`.
5. The engine connects, sends `session.update`, forwards audio chunks as base64 PCM appends, and receives transcript events concurrently.
6. For OpenAI, append-only deltas are emitted to `text_tx`.
7. For Lemonade, replaceable deltas update internal latest-interim state and are logged at debug level; non-empty completed transcripts are emitted once.
8. When whisrs stops recording or auto-stop closes audio, the engine sends `input_audio_buffer.commit` when the profile requires a final flush.
9. The engine waits for a post-commit completion or an error/timeout, then closes the WebSocket.
10. The existing daemon pipeline types emitted text, handles already-emitted text on errors, and saves history.

This fits the repo by preserving the existing daemon streaming API and moving provider protocol variation behind a small internal module rather than broadening the public trait.

## 5. Files and Responsibilities

Create:

- `src/transcription/openai_realtime_protocol.rs`
  - Shared message types, profiles, engine options, audio encoding/resampling, WebSocket lifecycle, event parsing, and protocol unit tests.
- `src/transcription/openai_compatible_realtime.rs`
  - New external backend wrapper and config-to-engine validation helpers.

Modify:

- `src/transcription/mod.rs`
  - Add `pub mod openai_realtime_protocol;`
  - Add `pub mod openai_compatible_realtime;`
- `src/transcription/openai_realtime.rs`
  - Reduce to OpenAI-specific wrapper over the shared engine.
  - Keep tests that assert OpenAI compatibility, moving protocol tests to the shared module.
- `src/lib.rs`
  - Add `OpenAiCompatibleRealtimeConfig`.
  - Add `Config.openai_compatible_realtime`.
  - Add defaults, validation, `has_any_backend_configured()`, and config tests.
- `src/daemon/main.rs`
  - Import and instantiate `OpenAiCompatibleRealtimeBackend`.
  - Add backend case to `create_backend()`.
  - Add backend case to `get_model_for_backend()`.
- `src/config/setup.rs`
  - Add backend choice and prompts for URL, model, profile, turn detection, optional API key.
  - Update tuple return type or replace it with a small `ConfiguredBackendSections` struct to avoid further tuple growth.
- `src/config/edit.rs`
  - Preserve and edit the new config section.
  - Show the external realtime backend as a sidecar/local backend with optional API key.
- `README.md`
  - Add the backend to the backend table and config summary.
- `docs/configuration.md`
  - Add `[openai-compatible-realtime]` reference.
- `contrib/asr-sidecars/README.md`
  - Mention that realtime OpenAI-compatible sidecars use a separate WebSocket backend, not the HTTP `asr-sidecar` contract.
- `specs/openai-compatible-realtime/README.md`
  - Keep as the proposal/spec and link to this design/task plan if useful.

Optional if integration tests become broad:

- `tests/openai_compatible_realtime.rs`
  - Mock WebSocket integration tests. Unit tests can also live inside `openai_realtime_protocol.rs` if the mock server is small.

## 6. Core APIs / Types / Classes

New config:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiCompatibleRealtimeConfig {
    pub url: String,
    #[serde(default = "default_openai_compatible_realtime_model")]
    pub model: String,
    #[serde(default = "default_openai_compatible_realtime_profile")]
    pub profile: String,
    #[serde(default = "default_openai_compatible_realtime_turn_detection")]
    pub turn_detection: String,
    #[serde(default)]
    pub api_key: Option<String>,
}
```

Shared protocol types:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRealtimeProfile {
    OpenAi,
    Lemonade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDetectionMode {
    ServerVad,
    ManualCommit,
}

#[derive(Debug, Clone)]
pub struct RealtimeEngineConfig {
    pub url: String,
    pub auth_bearer: Option<String>,
    pub host_header: Option<String>,
    pub profile: OpenAiRealtimeProfile,
    pub turn_detection: TurnDetectionMode,
}

pub struct OpenAiRealtimeProtocolEngine {
    config: RealtimeEngineConfig,
}

impl OpenAiRealtimeProtocolEngine {
    pub fn new(config: RealtimeEngineConfig) -> Self;

    pub async fn transcribe(
        &self,
        audio: &[u8],
        request: &TranscriptionConfig,
    ) -> anyhow::Result<String>;

    pub async fn transcribe_stream(
        &self,
        audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        request: &TranscriptionConfig,
    ) -> anyhow::Result<()>;
}
```

Profile helpers:

```rust
impl OpenAiRealtimeProfile {
    pub fn parse(value: &str) -> anyhow::Result<Self>;
    pub fn input_sample_rate(self) -> u32;
    pub fn delta_mode(self) -> DeltaMode;
    pub fn session_update(
        self,
        model: &str,
        language: &str,
        prompt: Option<&str>,
        turn_detection: TurnDetectionMode,
    ) -> anyhow::Result<serde_json::Value>;
    pub fn should_send_commit_on_eos(self, turn_detection: TurnDetectionMode) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaMode {
    AppendOnly,
    ReplaceableInterim,
}
```

Backend wrappers:

```rust
pub struct OpenAIRealtimeBackend {
    api_key: String,
}

pub struct OpenAiCompatibleRealtimeBackend {
    engine: OpenAiRealtimeProtocolEngine,
}

impl OpenAiCompatibleRealtimeBackend {
    pub fn new(config: OpenAiCompatibleRealtimeConfig) -> anyhow::Result<Self>;
}
```

The existing `OpenAIRealtimeBackend::new(api_key: String) -> Self` should remain so current daemon code and tests are not forced through config structs.

## 7. Data Model

Config TOML:

```toml
[general]
backend = "openai-compatible-realtime"
language = "auto"

[openai-compatible-realtime]
url = "ws://localhost:12345/realtime?model=Whisper-Tiny"
model = "Whisper-Tiny"
profile = "lemonade"
turn_detection = "server-vad"
# api_key = "optional bearer token"
```

Internal state:

- `RealtimeEngineConfig`
  - Immutable connection/profile options for a backend instance.
- `TranscriptionConfig`
  - Existing per-request language, model, and prompt.
- `StreamState`
  - Tracks whether input has ended, whether commit has been sent, emitted completed item IDs/texts, latest interim item text, and whether the final post-commit completion was observed.

Suggested internal stream state:

```rust
struct StreamState {
    input_closed: bool,
    commit_sent: bool,
    final_completion_seen: bool,
    latest_interim_by_item: std::collections::HashMap<String, String>,
    emitted_completed_text: std::collections::HashSet<String>,
    emitted_append_text: String,
}
```

If server messages include stable item IDs, prefer `HashSet<String>` of item IDs for duplicate suppression. If not, use trimmed transcript text as a conservative first-version fallback.

For replaceable providers such as Lemonade, `latest_interim_by_item` is not an
output buffer. It is only the most recent unstable hypothesis for each server
item, useful for debug logs and for a future daemon/UI that can replace text.
The only Lemonade text sent to `mpsc::Sender<String>` in this design is final
`completed` text.

Wire messages:

- Client:
  - `session.update`
  - `input_audio_buffer.append`
  - `input_audio_buffer.commit`
- Server:
  - `conversation.item.input_audio_transcription.delta`
  - `conversation.item.input_audio_transcription.completed`
  - `conversation.item.input_audio_transcription.failed`
  - `error`
  - session/audio buffer lifecycle events for logging.

## 8. Key Algorithms / Workflows

Session update construction:

1. Parse profile from config.
2. For `OpenAi`, serialize the existing nested transcription session:
   - `session.type = "transcription"`
   - `audio.input.format = { type = "audio/pcm", rate = 24000 }`
   - `audio.input.transcription.model`
   - omit `language` when it is `"auto"`
   - include clamped prompt only for models that support it
   - include server VAD unless model/manual mode requires manual commit.
3. For `Lemonade`, serialize the flat session shape:
   - `session.model`
   - `session.turn_detection = { type = "server_vad" }` for server VAD
   - omit turn detection or set null for manual commit, matching Lemonade behavior confirmed by tests/manual validation.
   - do not send undocumented prompt/language fields in the first version.

Audio send loop:

1. Receive `AudioChunk` from daemon pipeline.
2. If profile sample rate is 16 kHz, use chunk directly.
3. If profile sample rate is 24 kHz, resample 16 kHz to 24 kHz with the existing linear interpolation implementation.
4. Encode little-endian PCM16 bytes as base64.
5. Send `input_audio_buffer.append`.
6. When `audio_rx` closes, mark input closed.
7. If profile/mode requires final flush, send `input_audio_buffer.commit`.
8. Wait for final completion signal or timeout.
9. Send WebSocket close frame.

Receive loop:

1. Parse each text frame as a server event envelope.
2. For append-only deltas:
   - emit non-empty `delta` to `text_tx`.
   - append it to `emitted_append_text` for duplicate suppression.
3. For replaceable interim deltas:
   - log at debug level.
   - update `latest_interim_by_item` when an item ID is available.
   - do not emit to `text_tx`.
4. For completed transcripts:
   - trim and skip empty text.
   - clear any latest-interim state for the completed item.
   - if already emitted through append-only deltas or completed duplicate set, skip.
   - otherwise send text to `text_tx`.
   - if input is closed and commit has been sent, mark final completion seen.
   - if input is still open, keep receiving; this was a VAD item completion, not stream completion.
5. For `error` or `failed`:
   - return an error with sanitized endpoint context and server message.
   - preserve any text already emitted by allowing the daemon pipeline to keep typed text/history.
6. For session/audio buffer lifecycle events:
   - log and continue.
7. For close frames:
   - if final completion was expected but not seen, return a timeout/closed-before-final error.

Non-streaming `transcribe()` bridge:

1. Reject empty audio.
2. Decode WAV samples with `hound`.
3. Send samples through a bounded `mpsc` channel.
4. Drop the sender to trigger commit/finalization.
5. Collect `String` chunks from the streaming engine.
6. Join chunks with a space only when needed, matching current `openai_realtime.rs` behavior.

Backend creation:

1. `Config::validate()` rejects missing/invalid URL/profile/turn detection before daemon startup.
2. `create_backend()` constructs `OpenAiCompatibleRealtimeBackend`.
3. `get_model_for_backend()` returns `[openai-compatible-realtime].model`.
4. `supports_streaming()` returns true.

## 9. Errors and Edge Cases

Validation:

- Empty URL: fatal config error.
- URL scheme not `ws` or `wss`: fatal config error.
- Unknown profile: fatal config error listing `lemonade`.
- Unknown turn detection: fatal config error listing `server-vad` and `manual-commit`.
- Empty model: fatal config error.
- Empty non-streaming audio: backend error.

Failure cases:

- WebSocket handshake failure should include sanitized endpoint context.
- Server `error` and `conversation.item.input_audio_transcription.failed` should become backend errors.
- Malformed server JSON should be logged and ignored unless it prevents finalization repeatedly.
- Send failure after the server closes should end the stream and surface useful context.
- No final post-commit completion within timeout should warn or error depending on whether any completed text was emitted after input closed.
- Completed events before end-of-audio must never close the stream.
- Duplicate completed transcripts must not be typed twice.
- Lemonade replaceable deltas must not be appended to typed text; they may only update latest-interim state.
- OpenAI append deltas must continue to type incrementally.
- Lemonade will feel less immediate than OpenAI append-only streaming because text appears when the server finalizes a VAD turn or commit, not on every interim update.

Sensitive data:

- Log sanitized URLs. Strip userinfo and query string when logging external endpoints unless the query is known safe.
- Never log `api_key` or `Authorization` headers.
- Avoid raw message debug logs for messages that might contain credentials. Transcript event logs are acceptable at debug level but should avoid dumping full session config with auth.

## 10. Tests

Follow existing inline unit test patterns in `src/transcription/openai_realtime.rs`, `src/transcription/asr_sidecar.rs`, and `src/lib.rs`.

Suggested unit tests in `src/transcription/openai_realtime_protocol.rs`:

- OpenAI profile serializes the existing nested 24 kHz session update.
- OpenAI `"auto"` language is omitted as today.
- OpenAI prompt trimming/truncation remains at the existing 1024 character limit.
- OpenAI `gpt-realtime-whisper` or manual mode omits prompt/VAD as current behavior requires.
- Lemonade profile serializes flat `session.model`.
- Lemonade server VAD and manual commit variants serialize distinctly.
- Unknown profile and turn detection strings are rejected.
- Lemonade audio stays 16 kHz and is not resampled.
- OpenAI audio is resampled to 24 kHz.
- `input_audio_buffer.append` and `input_audio_buffer.commit` serialize correctly.
- Delta, completed, failed, error, session, and audio-buffer events parse correctly.
- Replaceable Lemonade deltas are not emitted to `text_tx`.
- Replaceable Lemonade deltas update latest-interim state without reaching `text_tx`.
- Multiple completed items before input close do not set final completion.
- Only a completed item after input close/commit satisfies final flush.
- Duplicate completed transcripts are suppressed.

Suggested tests in `src/transcription/openai_compatible_realtime.rs`:

- Constructor rejects empty URL.
- Constructor rejects `http://` URL.
- Constructor accepts `ws://` and `wss://`.
- Optional API key adds bearer auth without requiring one.

Suggested tests in `src/lib.rs`:

- TOML parses `[openai-compatible-realtime]` defaults.
- Config validation accepts a valid Lemonade config.
- Config validation rejects missing URL, unknown profile, and unknown turn detection.
- Unknown backend error message includes `openai-compatible-realtime`.
- `has_any_backend_configured()` returns true when the external realtime URL is configured.

Suggested integration test with a local mock WebSocket server:

1. Start a tokio WebSocket listener on localhost.
2. Instantiate `OpenAiCompatibleRealtimeBackend` with `profile = "lemonade"`.
3. Assert the first client message is the expected Lemonade `session.update`.
4. Feed two audio chunks and assert append messages arrive.
5. Send two completed transcripts while the audio channel is still open and verify both are emitted.
6. Close audio channel, assert commit arrives, send final completed transcript, and verify clean shutdown.
7. Repeat for server error and final-timeout behavior.

Manual Lemonade acceptance:

- Start Lemonade and resolve its realtime WebSocket port.
- Configure `backend = "openai-compatible-realtime"`.
- Dictate multiple phrases separated by silence without stopping whisrs.
- Verify each completed phrase is typed once while recording remains active.
- Verify interim Lemonade deltas are received/logged or buffered but do not appear at the cursor.
- Stop during speech and verify trailing speech flushes after commit.
- Verify debug logs show 16 kHz direct encoding for Lemonade and no 24 kHz resampling.

## 11. Acceptance Criteria

- [ ] `backend = "openai-compatible-realtime"` works with a Lemonade realtime WebSocket URL.
- [ ] Existing `backend = "openai-realtime"` config continues to work without user changes.
- [ ] OpenAI cloud and external realtime backends share one protocol module and one set of OpenAI-compatible message types.
- [ ] No Lemonade-specific WebSocket send/receive loop exists outside the shared protocol engine.
- [ ] Existing batch `asr-sidecar` HTTP behavior remains unchanged.
- [ ] Lemonade uses 16 kHz PCM directly; OpenAI cloud still receives 24 kHz PCM.
- [ ] Server-VAD completed items during recording do not terminate the WebSocket stream.
- [ ] End-of-audio sends commit when needed and waits for a post-commit completion or timeout.
- [ ] Lemonade replaceable interim deltas are not typed.
- [ ] Lemonade interim deltas can be received without cursor output; only completed transcripts are typed in v1.
- [ ] Completed transcripts are typed once and duplicate completed text is suppressed.
- [ ] Config validation catches empty URL, non-WebSocket URL, unknown profile, unknown turn detection, and empty model.
- [ ] Setup/config editor can create and preserve `[openai-compatible-realtime]`.
- [ ] Documentation lists the new backend and distinguishes it from the HTTP sidecar contract.
- [ ] `cargo fmt -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo build` pass after implementation.
