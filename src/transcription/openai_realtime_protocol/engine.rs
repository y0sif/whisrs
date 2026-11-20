use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;
use tracing::{debug, error, info, warn};

use crate::audio::AudioChunk;
use crate::transcription::{needs_space_between, StreamText, TranscriptionConfig};

use super::profile::{
    encode_pcm_base64, resample_16k_to_24k, DeltaMode, OpenAiRealtimeProfile, TurnDetectionMode,
};
use super::wire::{AudioBufferAppend, AudioBufferCommit, ServerMessage};

const FINAL_COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// Short post-commit wait used once we have already typed at least one
// transcript this session. Server VAD can finalize the last turn before
// recording stops, leaving the trailing end-of-stream commit with nothing to
// flush and no post-commit completion to follow. Since we already have output
// (and for append-only profiles any final completion is a duplicate that gets
// deduped anyway), we only grant a brief grace for a real completion before
// finalizing gracefully, instead of stalling the daemon for the full timeout.
// A profile whose server completes every commit waits for that completion.
const POST_COMMIT_GRACE_AFTER_TEXT: std::time::Duration = std::time::Duration::from_secs(2);

/// Immutable connection options for the shared realtime engine.
#[derive(Debug, Clone)]
pub struct RealtimeEngineConfig {
    pub url: String,
    pub endpoint_display: String,
    pub auth_bearer: Option<String>,
    pub host_header: Option<String>,
    pub profile: OpenAiRealtimeProfile,
    pub turn_detection: TurnDetectionMode,
    pub languages: Vec<String>,
    pub final_completion_timeout: Option<std::time::Duration>,
}

/// Shared OpenAI-compatible realtime protocol engine.
#[derive(Debug, Clone)]
pub struct OpenAiRealtimeProtocolEngine {
    config: RealtimeEngineConfig,
}

#[derive(Debug, Default)]
struct StreamState {
    // Set once the producer side has finished sending audio chunks.
    input_closed: bool,
    // Set after we explicitly flush buffered audio with input_audio_buffer.commit.
    commit_sent: bool,
    // Set after we observe the completion event that corresponds to the
    // committed tail of the stream.
    final_completion_seen: bool,
    // For providers that send replaceable interim text, keep only the latest
    // interim string per item so later events can supersede earlier ones.
    latest_interim_by_item: HashMap<String, String>,
    // Completed events can occasionally be replayed by the provider. Prefer
    // item IDs when present because they are the most stable duplicate key.
    emitted_completed_item_ids: HashSet<String>,
    // When no stable item ID is available, fall back to a normalized transcript
    // string so we do not type the same completed utterance twice.
    emitted_completed_text: HashSet<String>,
    // Append-only providers stream stable text through delta events. Keep a
    // running copy: an item's first delta is spaced from it.
    emitted_append_text: String,
    // The item (utterance) the last append-only delta with an item ID belonged
    // to, so the first delta of the next one can be spaced from the text before.
    last_append_item_id: Option<String>,
    // Set once we have forwarded at least one transcript to the downstream
    // typing channel. Used to decide whether a missing post-commit completion is
    // a graceful finalize or a genuine failure. Only flipped when text is
    // actually emitted downstream — not on empty/duplicate completions or on
    // buffered Lemonade interim deltas that are never typed.
    emitted_any_text: bool,
}

#[derive(Debug)]
struct DeltaAction {
    emit_text: Option<String>,
}

#[derive(Debug)]
struct CompletedAction {
    emit_text: Option<String>,
    finalizes_stream: bool,
}

impl StreamState {
    // Normalize transcript text for duplicate suppression without mutating the
    // user-visible output we forward downstream.
    fn normalized_transcript_key(text: &str) -> Option<String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn awaiting_post_commit_completion(&self) -> bool {
        self.input_closed && self.commit_sent && !self.final_completion_seen
    }

    // When input has already closed and we have already typed at least one
    // transcript this session, a missing post-commit completion (timeout, clean
    // close, or empty-commit error) is not a real failure: server VAD can finish
    // the final turn before recording stops, leaving the trailing commit with
    // nothing to flush. In that case we finalize gracefully and keep the typed
    // text instead of erroring into the recovery-save path. When nothing was
    // ever emitted, the original error/bail behavior is kept so genuine failures
    // still surface.
    fn can_finalize_without_post_commit_completion(&self) -> bool {
        self.input_closed && self.emitted_any_text
    }

    fn note_input_closed(&mut self) {
        self.input_closed = true;
    }

    fn note_commit_sent(&mut self) {
        self.commit_sent = true;
    }

    fn on_delta(
        &mut self,
        profile: OpenAiRealtimeProfile,
        item_id: Option<&str>,
        delta: String,
    ) -> DeltaAction {
        match profile.delta_mode() {
            // OpenAI-style deltas are append-only, so forwarding them downstream
            // preserves the text stream exactly as emitted by the provider,
            // except at an item boundary: server VAD splits a dictation into
            // several items, and an item's first delta has no leading space. It
            // gets one by `needs_space_between` when an item ID follows a
            // different one. A delta without an item ID never gets one and
            // leaves the tracked item as it is, and neither does the first
            // delta with an ID, which has no item before it to space from.
            DeltaMode::AppendOnly => {
                let new_item = match (self.last_append_item_id.as_deref(), item_id) {
                    (Some(last), Some(id)) => last != id,
                    _ => false,
                };
                if item_id.is_some() {
                    self.last_append_item_id = item_id.map(str::to_string);
                }
                let text = if new_item && needs_space_between(&self.emitted_append_text, &delta) {
                    format!(" {delta}")
                } else {
                    delta
                };
                self.emitted_append_text.push_str(&text);
                self.emitted_any_text = true;
                DeltaAction {
                    emit_text: Some(text),
                }
            }
            DeltaMode::ReplaceableInterim => {
                // Lemonade-style deltas are replaceable previews, not stable
                // transcript segments. We keep the latest value in memory and
                // wait for the completed event before emitting text downstream.
                if let Some(item_id) = item_id {
                    self.latest_interim_by_item
                        .insert(item_id.to_string(), delta);
                }
                DeltaAction { emit_text: None }
            }
        }
    }

    fn on_completed(
        &mut self,
        profile: OpenAiRealtimeProfile,
        item_id: Option<&str>,
        transcript: Option<String>,
    ) -> CompletedAction {
        // A completed event supersedes any tracked interim text for the same item.
        if let Some(item_id) = item_id {
            self.latest_interim_by_item.remove(item_id);
        }

        // v1 Lemonade behavior is "completed-utterance realtime": we receive
        // interim previews, but only stable completed text reaches the daemon's
        // append-only String channel.
        let emit_text = match profile.delta_mode() {
            // For append-only providers, the transcript has already been emitted
            // incrementally via delta messages, so re-emitting here would duplicate text.
            DeltaMode::AppendOnly => None,
            // For replaceable-interim providers, completed is the first stable
            // transcript we should pass to the daemon pipeline.
            DeltaMode::ReplaceableInterim => transcript
                .and_then(|text| Self::normalized_transcript_key(&text).map(|_| text))
                .filter(|text| {
                    if let Some(item_id) = item_id {
                        if self.emitted_completed_item_ids.contains(item_id) {
                            return false;
                        }
                    }

                    let normalized = match Self::normalized_transcript_key(text) {
                        Some(normalized) => normalized,
                        None => return false,
                    };

                    // When a provider offers both append-only deltas and a later
                    // completed transcript for the same item, skip the completed
                    // event if the stable text is already fully present.
                    if self.emitted_append_text.contains(&normalized) {
                        return false;
                    }

                    !self.emitted_completed_text.contains(&normalized)
                }),
        };

        if let Some(item_id) = item_id {
            if emit_text.is_some() {
                self.emitted_completed_item_ids.insert(item_id.to_string());
            }
        }
        if let Some(text) = &emit_text {
            if let Some(normalized) = Self::normalized_transcript_key(text) {
                self.emitted_completed_text.insert(normalized);
            }
            // Only record that we forwarded text when a non-empty, non-duplicate
            // completed transcript actually reaches the typing channel.
            self.emitted_any_text = true;
        }

        // The stream only becomes terminal once we have closed input, sent an
        // explicit commit, and received the completion for that committed tail.
        let finalizes_stream = self.awaiting_post_commit_completion();
        if finalizes_stream {
            self.final_completion_seen = true;
        }

        CompletedAction {
            emit_text,
            finalizes_stream,
        }
    }
}

impl OpenAiRealtimeProtocolEngine {
    pub fn new(config: RealtimeEngineConfig) -> Self {
        Self { config }
    }

    fn final_completion_timeout(&self) -> std::time::Duration {
        self.config
            .final_completion_timeout
            .unwrap_or(FINAL_COMPLETION_TIMEOUT)
    }

    // Grace period to wait for a real post-commit completion once text was
    // already emitted. Never longer than the configured final-completion timeout
    // so tests that pin a tiny timeout keep their tight bounds.
    fn post_commit_grace_timeout(&self) -> std::time::Duration {
        self.final_completion_timeout()
            .min(POST_COMMIT_GRACE_AFTER_TEXT)
    }

    async fn connect(
        &self,
    ) -> anyhow::Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        info!(
            "connecting to realtime transcription endpoint: {}",
            self.config.endpoint_display
        );

        let mut builder = tungstenite::http::Request::builder()
            .uri(&self.config.url)
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .header("Sec-WebSocket-Version", "13")
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket");

        if let Some(auth_bearer) = &self.config.auth_bearer {
            builder = builder.header("Authorization", format!("Bearer {auth_bearer}"));
        }
        if let Some(host_header) = &self.config.host_header {
            builder = builder.header("Host", host_header);
        } else if let Some(host_header) = host_header_from_url(&self.config.url) {
            builder = builder.header("Host", host_header);
        }

        let request = builder.body(())?;
        let (ws_stream, _response) = tokio_tungstenite::connect_async(request).await?;
        Ok(ws_stream)
    }

    pub async fn transcribe(
        &self,
        audio: &[u8],
        request: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        if audio.is_empty() {
            anyhow::bail!("cannot transcribe empty audio");
        }

        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(16);
        let (text_tx, mut text_rx) = mpsc::channel::<String>(16);

        let cursor = std::io::Cursor::new(audio);
        let reader = hound::WavReader::new(cursor)?;
        let samples: Vec<i16> = reader.into_samples::<i16>().collect::<Result<_, _>>()?;

        audio_tx.send(samples).await.ok();
        drop(audio_tx);

        let request_clone = request.clone();
        let stream_result = self.transcribe_stream(audio_rx, text_tx, &request_clone);

        // Append deltas carry their own spacing (the engine spaces one item
        // from the next), so they join verbatim. Lemonade's completed
        // utterances are phrases and get a space between them.
        let spaced = self.config.profile.stream_text() == StreamText::Phrases;
        let collector = async {
            let mut full_text = String::new();
            while let Some(text) = text_rx.recv().await {
                if spaced && !full_text.is_empty() {
                    full_text.push(' ');
                }
                full_text.push_str(&text);
            }
            full_text
        };

        let (stream_res, text) = tokio::join!(stream_result, collector);
        stream_res?;
        Ok(text)
    }

    pub async fn transcribe_stream(
        &self,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        request: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        let ws_stream = self.connect().await?;
        let (mut ws_sink, mut ws_source) = ws_stream.split();

        info!("connected to realtime transcription endpoint");

        let session_update = self.config.profile.session_update(
            &request.model,
            &request.language,
            &self.config.languages,
            request.prompt.as_deref(),
            self.config.turn_detection,
        )?;
        let session_json = serde_json::to_string(&session_update)?;
        ws_sink
            .send(tungstenite::Message::Text(session_json.into()))
            .await?;
        debug!(
            "sent session.update for profile={:?}, model={}",
            self.config.profile, request.model
        );

        // The state machine here has one main job: keep streaming audio until
        // the caller closes input, then decide whether the provider requires an
        // explicit commit and a post-commit completion before the stream can end.
        let profile = self.config.profile;
        let turn_detection = self.config.turn_detection;
        let mut state = StreamState::default();
        let stream_result: anyhow::Result<()> = async {
            loop {
                if state.awaiting_post_commit_completion() {
                    // Once we have committed the buffered tail, stop reading from
                    // the audio channel and wait specifically for the final
                    // completion event. This prevents us from returning early on
                    // providers that emit multiple completed events over a session.
                    //
                    // Timeout selection: if we have already typed at least one
                    // transcript, a missing post-commit completion is benign
                    // (server VAD finalized the last turn before stop), so we only
                    // grant a short grace period before finalizing gracefully and
                    // never stall the daemon for the full timeout. If nothing has
                    // been emitted yet we are still waiting for the only result,
                    // so we keep the full FINAL_COMPLETION_TIMEOUT. A profile whose
                    // server completes every commit keeps it too: that completion
                    // always comes, and a server slower than real time can be
                    // quiet for longer than the grace while it catches up.
                    let post_commit_timeout = if state.emitted_any_text
                        && !profile.completes_every_commit()
                    {
                        self.post_commit_grace_timeout()
                    } else {
                        self.final_completion_timeout()
                    };

                    let timed_out =
                        tokio::time::timeout(post_commit_timeout, ws_source.next()).await;

                    let msg_result = match timed_out {
                        Ok(msg_result) => msg_result,
                        Err(_) => {
                            // A post-commit timeout only fails the stream if we
                            // never typed anything; otherwise keep the text.
                            if state.can_finalize_without_post_commit_completion() {
                                warn!(
                                    "no post-commit completion from {} within grace period; finalizing with already-typed transcript",
                                    self.config.endpoint_display
                                );
                                break;
                            }
                            anyhow::bail!(
                                "timed out waiting for final transcription completion from {} after commit",
                                self.config.endpoint_display
                            );
                        }
                    };

                    let Some(msg_result) = msg_result else {
                        // Clean close with no post-commit completion: finalize
                        // gracefully if we already typed text, else surface it.
                        if state.can_finalize_without_post_commit_completion() {
                            warn!(
                                "realtime endpoint {} closed before post-commit completion; finalizing with already-typed transcript",
                                self.config.endpoint_display
                            );
                            break;
                        }
                        anyhow::bail!(
                            "realtime transcription endpoint {} closed before final completion after commit",
                            self.config.endpoint_display
                        );
                    };

                    if self
                        .handle_server_message(msg_result, &text_tx, profile, &mut state)
                        .await?
                    {
                        break;
                    }
                    continue;
                }

                tokio::select! {
                    maybe_chunk = audio_rx.recv(), if !state.input_closed => {
                        match maybe_chunk {
                            Some(chunk) => {
                                // Normal steady-state: encode the daemon chunk into
                                // the provider's expected wire format and push it out.
                                self.send_audio_chunk(&mut ws_sink, profile, chunk).await?;
                            }
                            None => {
                                // End-of-stream from the caller means "flush whatever
                                // audio is already buffered." Some providers do that
                                // only after an explicit commit; others finalize on
                                // server-side VAD and do not want a trailing commit here.
                                state.note_input_closed();
                                if profile.should_send_commit_on_eos(turn_detection) {
                                    self.send_commit(&mut ws_sink).await?;
                                    state.note_commit_sent();
                                } else {
                                    // No explicit commit path means there is no extra
                                    // provider-managed tail to wait for after input ends.
                                    state.final_completion_seen = true;
                                    break;
                                }
                            }
                        }
                    }
                    msg = ws_source.next() => {
                        let Some(msg_result) = msg else {
                            // The distinction matters for diagnostics: closing while
                            // audio is still arriving is different from closing after
                            // input ended but before the provider produced its final turn.
                            if state.input_closed {
                                // Closed after input ended without a post-commit
                                // completion: finalize gracefully if we already
                                // typed text, else surface the failure.
                                if state.can_finalize_without_post_commit_completion() {
                                    warn!(
                                        "realtime endpoint {} closed after input ended without a post-commit completion; finalizing with already-typed transcript",
                                        self.config.endpoint_display
                                    );
                                    break;
                                }
                                anyhow::bail!(
                                    "realtime transcription endpoint {} closed before final completion after commit",
                                    self.config.endpoint_display
                                );
                            }
                            anyhow::bail!(
                                "realtime transcription endpoint {} closed while audio was still streaming",
                                self.config.endpoint_display
                            );
                        };

                        if self
                            .handle_server_message(msg_result, &text_tx, profile, &mut state)
                            .await?
                        {
                            break;
                        }
                    }
                }
            }

            Ok(())
        }
        .await;

        ws_sink.send(tungstenite::Message::Close(None)).await.ok();
        if stream_result.is_ok() {
            info!("realtime transcription stream finished");
        }
        stream_result
    }

    async fn send_audio_chunk(
        &self,
        ws_sink: &mut futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tungstenite::Message,
        >,
        profile: OpenAiRealtimeProfile,
        chunk: AudioChunk,
    ) -> anyhow::Result<()> {
        let prepared = prepare_audio_chunk_for_profile(profile, &chunk)?;
        let json = serde_json::to_string(&AudioBufferAppend::new(encode_pcm_base64(&prepared)))?;
        ws_sink
            .send(tungstenite::Message::Text(json.into()))
            .await?;
        Ok(())
    }

    async fn send_commit(
        &self,
        ws_sink: &mut futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tungstenite::Message,
        >,
    ) -> anyhow::Result<()> {
        debug!("committing audio buffer for transcription");
        let json = serde_json::to_string(&AudioBufferCommit::new())?;
        ws_sink
            .send(tungstenite::Message::Text(json.into()))
            .await?;
        Ok(())
    }

    async fn handle_server_message(
        &self,
        msg_result: Result<tungstenite::Message, tungstenite::Error>,
        text_tx: &mpsc::Sender<String>,
        profile: OpenAiRealtimeProfile,
        state: &mut StreamState,
    ) -> anyhow::Result<bool> {
        match msg_result {
            Ok(tungstenite::Message::Text(text)) => {
                match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(server_msg) => match server_msg.msg_type.as_str() {
                        "conversation.item.input_audio_transcription.delta" => {
                            if let Some(delta) = server_msg.delta {
                                if delta.is_empty() {
                                    return Ok(false);
                                }
                                let delta_preview = delta.clone();
                                // Provider profiles decide whether a delta is a stable
                                // append-only token stream or a replaceable interim preview.
                                let action =
                                    state.on_delta(profile, server_msg.item_id.as_deref(), delta);
                                if let Some(text) = action.emit_text {
                                    debug!("realtime delta: {text}");
                                    if let Err(e) = text_tx.send(text).await {
                                        debug!(
                                            "dropping append-only realtime delta because text channel is unavailable: {e}"
                                        );
                                    }
                                } else if server_msg.item_id.is_none() {
                                    debug!(
                                        "replaceable realtime delta buffered without item_id: {}",
                                        delta_preview
                                    );
                                } else {
                                    debug!("replaceable realtime delta buffered");
                                }
                            }
                        }
                        "conversation.item.input_audio_transcription.completed" => {
                            if let Some(transcript) = &server_msg.transcript {
                                debug!("realtime completed: {transcript}");
                            }
                            // Completed is both a content event and, after an explicit
                            // commit, the signal that the provider has finished the tail
                            // of the current stream.
                            let action = state.on_completed(
                                profile,
                                server_msg.item_id.as_deref(),
                                server_msg.transcript,
                            );
                            if let Some(text) = action.emit_text {
                                if let Err(e) = text_tx.send(text).await {
                                    warn!(
                                        "dropping completed realtime transcript because text channel is unavailable: {e}"
                                    );
                                }
                            }
                            if action.finalizes_stream {
                                debug!("final post-commit completion observed");
                                return Ok(true);
                            }
                        }
                        "error" | "conversation.item.input_audio_transcription.failed" => {
                            let err_msg = server_msg
                                .error
                                .map(|e| e.message)
                                .unwrap_or_else(|| "unknown error".to_string());
                            // An empty-commit error after input closed means the
                            // final turn was already completed before recording
                            // stopped, so the trailing commit had nothing to
                            // flush. If we already typed a transcript, treat this
                            // as a graceful finalize rather than a failure.
                            if is_empty_commit_error(&err_msg)
                                && state.can_finalize_without_post_commit_completion()
                            {
                                warn!(
                                    "realtime endpoint {} reported an empty-commit after input ended ({err_msg}); finalizing with already-typed transcript",
                                    self.config.endpoint_display
                                );
                                state.final_completion_seen = true;
                                return Ok(true);
                            }
                            error!("realtime transcription error: {err_msg}");
                            debug!("raw error message: {text}");
                            anyhow::bail!(
                                "realtime transcription failed for {}: {err_msg}",
                                self.config.endpoint_display
                            );
                        }
                        "session.created"
                        | "session.updated"
                        | "transcription_session.created"
                        | "transcription_session.updated" => {
                            debug!("session event: {}", server_msg.msg_type);
                        }
                        "input_audio_buffer.committed"
                        | "input_audio_buffer.speech_started"
                        | "input_audio_buffer.speech_stopped" => {
                            debug!("audio buffer event: {}", server_msg.msg_type);
                        }
                        "input_audio_buffer.cleared" => {
                            debug!("audio buffer event: {}", server_msg.msg_type);
                            // Lemonade may clear the committed buffer without
                            // emitting a second completed item when the last
                            // utterance already finalized before stop. In that
                            // case, treat buffer-cleared as the terminal post-
                            // commit flush signal instead of waiting until the
                            // timeout fires.
                            if profile == OpenAiRealtimeProfile::Lemonade
                                && state.awaiting_post_commit_completion()
                            {
                                debug!("post-commit buffer cleared observed; finalizing stream");
                                state.final_completion_seen = true;
                                return Ok(true);
                            }
                        }
                        other => {
                            debug!("unhandled server message type: {other}");
                        }
                    },
                    Err(e) => {
                        // Some providers may emit extra compatibility fields or
                        // non-transcription events we do not model yet. Keep the
                        // socket alive unless the message is explicitly an error.
                        debug!("failed to parse server message: {e}");
                    }
                }
            }
            Ok(tungstenite::Message::Close(_)) => {
                if state.awaiting_post_commit_completion() {
                    // Server-initiated close before the post-commit completion:
                    // finalize gracefully if we already typed text, else surface
                    // the failure so genuine problems still reach the user.
                    if state.can_finalize_without_post_commit_completion() {
                        warn!(
                            "realtime endpoint {} sent close before post-commit completion; finalizing with already-typed transcript",
                            self.config.endpoint_display
                        );
                        state.final_completion_seen = true;
                        return Ok(true);
                    }
                    anyhow::bail!(
                        "realtime transcription endpoint {} closed before final completion after commit",
                        self.config.endpoint_display
                    );
                }
                info!("WebSocket closed by server");
                return Ok(true);
            }
            Err(e) => {
                anyhow::bail!(
                    "WebSocket receive error from {}: {e}",
                    self.config.endpoint_display
                );
            }
            _ => {}
        }

        Ok(false)
    }
}

// Recognize the family of "you committed an empty / already-flushed audio
// buffer" errors that OpenAI-compatible servers raise when server VAD finished
// the final turn before the client's end-of-stream commit. Matching is
// conservative and case-insensitive so it cannot swallow genuine failures such
// as auth or decoder errors.
fn is_empty_commit_error(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("commit_empty")
        || lowered.contains("buffer is empty")
        || lowered.contains("buffer too small")
        || lowered.contains("empty audio buffer")
}

fn host_header_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let header = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    Some(header)
}

// Profiles differ at the wire boundary: OpenAI wants 24 kHz PCM while
// Lemonade can consume the daemon's native 16 kHz chunks directly. Keep this
// as a small helper so tests can verify the exact pre-encoding behavior the
// websocket send path uses.
fn prepare_audio_chunk_for_profile(
    profile: OpenAiRealtimeProfile,
    chunk: &[i16],
) -> anyhow::Result<Vec<i16>> {
    match profile.input_sample_rate() {
        16_000 => Ok(chunk.to_vec()),
        24_000 => Ok(resample_16k_to_24k(chunk)),
        rate => anyhow::bail!("unsupported realtime input sample rate: {rate}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Lemonade completed event is typed either way, but finalizes the
    /// stream only once input closed and the commit went out.
    #[test]
    fn lemonade_completed_finalizes_only_after_close_and_commit() {
        for (name, closed_and_committed, want_finalizes) in [
            ("before input close", false, false),
            ("after close and commit", true, true),
        ] {
            let mut state = StreamState::default();
            if closed_and_committed {
                state.note_input_closed();
                state.note_commit_sent();
            }

            let action = state.on_completed(
                OpenAiRealtimeProfile::Lemonade,
                Some("item-1"),
                Some("hello".to_string()),
            );

            assert_eq!(action.emit_text.as_deref(), Some("hello"), "{name}");
            assert_eq!(action.finalizes_stream, want_finalizes, "{name}");
            assert_eq!(state.final_completion_seen, want_finalizes, "{name}");
        }
    }

    #[test]
    fn replaceable_delta_updates_latest_interim_by_item() {
        let mut state = StreamState::default();

        let action = state.on_delta(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            "hello wor".to_string(),
        );

        assert!(action.emit_text.is_none());
        assert_eq!(
            state
                .latest_interim_by_item
                .get("item-1")
                .map(String::as_str),
            Some("hello wor")
        );
    }

    #[test]
    fn lemonade_audio_chunk_preparation_keeps_16k_samples_unchanged() {
        let input: Vec<i16> = vec![1, -2, 3, -4, 5, -6, 7, -8];

        let prepared =
            prepare_audio_chunk_for_profile(OpenAiRealtimeProfile::Lemonade, &input).unwrap();

        assert_eq!(prepared, input);
    }

    #[test]
    fn openai_audio_chunk_preparation_resamples_to_24k() {
        let input: Vec<i16> = vec![100; 16];

        let prepared =
            prepare_audio_chunk_for_profile(OpenAiRealtimeProfile::OpenAi, &input).unwrap();

        assert_eq!(prepared.len(), 24);
        assert_ne!(prepared.len(), input.len());
    }

    /// Lemonade's interim deltas build up to the final text and emit
    /// nothing: only the completed event does, and it clears the interim.
    #[test]
    fn completed_clears_latest_interim_state() {
        let mut state = StreamState::default();
        for delta in ["hel", "hello wor", "hello world"] {
            let action = state.on_delta(
                OpenAiRealtimeProfile::Lemonade,
                Some("item-1"),
                delta.to_string(),
            );
            assert!(action.emit_text.is_none(), "delta {delta:?} was emitted");
        }

        let action = state.on_completed(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            Some("hello world".to_string()),
        );

        assert_eq!(action.emit_text.as_deref(), Some("hello world"));
        assert!(!state.latest_interim_by_item.contains_key("item-1"));
    }

    #[test]
    fn append_only_completed_does_not_emit_text() {
        let mut state = StreamState::default();
        let action = state.on_completed(
            OpenAiRealtimeProfile::OpenAi,
            Some("item-1"),
            Some("hello".to_string()),
        );

        assert!(action.emit_text.is_none());
        assert!(!action.finalizes_stream);
    }

    /// gpt-4o-transcribe sends a dictation as several items, each starting
    /// without a space: the next item's first delta gets one, the session's
    /// first delta and an item's continuation deltas none. Every step is
    /// emitted at once, and the running copy holds what was emitted.
    #[test]
    fn append_only_deltas_are_spaced_only_at_an_item_boundary() {
        // Each step: the delta's item ID, the delta, what it emits.
        type Step<'a> = (Option<&'a str>, &'a str, &'a str);
        let scenarios: [(&str, &[Step], &str); 4] = [
            (
                "new item spaced",
                &[
                    (Some("item-1"), "Сегодня", "Сегодня"),
                    (Some("item-1"), " завтра.", " завтра."),
                    (Some("item-2"), "П", " П"),
                    (Some("item-2"), "отом", "отом"),
                ],
                "Сегодня завтра. Потом",
            ),
            (
                "already spaced or punctuation",
                &[
                    (Some("item-1"), "Hello", "Hello"),
                    (Some("item-2"), " there", " there"),
                    (Some("item-3"), ".", "."),
                ],
                "Hello there.",
            ),
            (
                // Still item-1 at the end: the delta without an ID left the
                // tracked item alone.
                "delta without id",
                &[
                    (Some("item-1"), "Hel", "Hel"),
                    (None, "lo", "lo"),
                    (Some("item-1"), "ween", "ween"),
                ],
                "Helloween",
            ),
            (
                // The first ID after deltas without one is no boundary either.
                "first id after none",
                &[
                    (None, "Hel", "Hel"),
                    (Some("item-1"), "lo", "lo"),
                    (Some("item-2"), "World", " World"),
                ],
                "Hello World",
            ),
        ];
        for (name, steps, want_text) in scenarios {
            let mut state = StreamState::default();
            for (step, &(item_id, delta, want)) in steps.iter().enumerate() {
                let emitted = state
                    .on_delta(OpenAiRealtimeProfile::OpenAi, item_id, delta.to_string())
                    .emit_text
                    .unwrap_or_else(|| panic!("{name}, step {step}: nothing emitted"));
                assert_eq!(emitted, want, "{name}, step {step}");
            }
            assert_eq!(state.emitted_append_text, want_text, "{name}");
        }
    }

    /// A replayed Lemonade completed event is typed once: deduplicated by its
    /// item ID, or without one by its trimmed text.
    #[test]
    fn lemonade_completed_is_emitted_once() {
        for (name, item_id, replay) in [
            ("same item id", Some("item-1"), "hello world"),
            ("no item id, trimmed text", None, "  hello world  "),
        ] {
            let mut state = StreamState::default();

            let first = state.on_completed(
                OpenAiRealtimeProfile::Lemonade,
                item_id,
                Some("hello world".to_string()),
            );
            let second = state.on_completed(
                OpenAiRealtimeProfile::Lemonade,
                item_id,
                Some(replay.into()),
            );

            assert_eq!(first.emit_text.as_deref(), Some("hello world"), "{name}");
            assert!(second.emit_text.is_none(), "{name}: the replay was typed");
        }
    }

    #[test]
    fn completed_is_not_re_emitted_if_append_only_delta_already_sent_same_text() {
        let mut state = StreamState {
            emitted_append_text: "hello world".to_string(),
            ..StreamState::default()
        };

        let action = state.on_completed(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            Some("hello world".to_string()),
        );

        assert!(action.emit_text.is_none());
    }

    /// `emitted_any_text` is set only by text forwarded to the typing channel:
    /// not by a blank completed event, nor by a replaceable interim preview,
    /// which is buffered and never typed downstream.
    #[test]
    fn emitted_any_text_is_set_only_by_text_that_reaches_the_channel() {
        type Event = fn(&mut StreamState);
        let cases: [(&str, Event, bool); 4] = [
            (
                "append-only delta",
                |state| {
                    state.on_delta(
                        OpenAiRealtimeProfile::OpenAi,
                        Some("item-1"),
                        "hello".to_string(),
                    );
                },
                true,
            ),
            (
                "lemonade completed",
                |state| {
                    state.on_completed(
                        OpenAiRealtimeProfile::Lemonade,
                        Some("item-1"),
                        Some("hello world".to_string()),
                    );
                },
                true,
            ),
            (
                "blank completed",
                |state| {
                    state.on_completed(
                        OpenAiRealtimeProfile::Lemonade,
                        Some("item-1"),
                        Some("   ".to_string()),
                    );
                },
                false,
            ),
            (
                "buffered interim delta",
                |state| {
                    state.on_delta(
                        OpenAiRealtimeProfile::Lemonade,
                        Some("item-1"),
                        "hello wor".to_string(),
                    );
                },
                false,
            ),
        ];
        for (name, event, want) in cases {
            let mut state = StreamState::default();
            event(&mut state);
            assert_eq!(state.emitted_any_text, want, "{name}");
        }
    }

    /// A missing post-commit completion is a graceful finalize only once input
    /// closed and a transcript was already typed. Nothing typed is a genuine
    /// failure; text typed while input is still open means keep listening.
    #[test]
    fn graceful_finalize_needs_closed_input_and_typed_text() {
        for (name, closed, typed, want) in [
            ("nothing yet", false, false, false),
            ("input closed, nothing typed", true, false, false),
            ("typed, input open", false, true, false),
            ("input closed and typed", true, true, true),
        ] {
            let mut state = StreamState::default();
            if closed {
                state.note_input_closed();
            }
            if typed {
                state.on_completed(
                    OpenAiRealtimeProfile::Lemonade,
                    Some("item-1"),
                    Some("hello world".to_string()),
                );
            }
            assert_eq!(
                state.can_finalize_without_post_commit_completion(),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn empty_commit_errors_are_recognized() {
        assert!(is_empty_commit_error("input_audio_buffer_commit_empty"));
        assert!(is_empty_commit_error("Error: COMMIT_EMPTY"));
        assert!(is_empty_commit_error("the audio buffer is empty"));
        assert!(is_empty_commit_error("audio buffer too small to commit"));
        assert!(!is_empty_commit_error("bad auth"));
        assert!(!is_empty_commit_error("decoder failed"));
    }

    #[tokio::test]
    async fn lemonade_buffer_cleared_after_commit_finalizes_stream() {
        let engine = OpenAiRealtimeProtocolEngine::new(RealtimeEngineConfig {
            url: "ws://localhost:1234/realtime".to_string(),
            endpoint_display: "ws://localhost:1234/realtime".to_string(),
            auth_bearer: None,
            host_header: None,
            profile: OpenAiRealtimeProfile::Lemonade,
            turn_detection: TurnDetectionMode::ServerVad,
            languages: Vec::new(),
            final_completion_timeout: None,
        });
        let (text_tx, _text_rx) = mpsc::channel::<String>(1);
        let mut state = StreamState::default();
        state.note_input_closed();
        state.note_commit_sent();

        let finalized = engine
            .handle_server_message(
                Ok(tungstenite::Message::Text(
                    r#"{"type":"input_audio_buffer.cleared"}"#.to_string().into(),
                )),
                &text_tx,
                OpenAiRealtimeProfile::Lemonade,
                &mut state,
            )
            .await
            .unwrap();

        assert!(finalized);
        assert!(state.final_completion_seen);
    }
}

#[cfg(test)]
mod stream_lifecycle_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::{timeout, Duration, Instant};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    fn test_request() -> TranscriptionConfig {
        TranscriptionConfig {
            language: "en".to_string(),
            model: "gpt-4o-mini-transcribe".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        }
    }

    // Use the real 30s FINAL_COMPLETION_TIMEOUT (final_completion_timeout: None)
    // so we exercise the production short-grace path: once text was emitted the
    // engine must finalize within POST_COMMIT_GRACE_AFTER_TEXT, NOT wait 30s.
    fn openai_engine_default_timeout(url: String) -> OpenAiRealtimeProtocolEngine {
        OpenAiRealtimeProtocolEngine::new(RealtimeEngineConfig {
            endpoint_display: url.clone(),
            url,
            auth_bearer: None,
            host_header: None,
            profile: OpenAiRealtimeProfile::OpenAi,
            turn_detection: TurnDetectionMode::ServerVad,
            languages: Vec::new(),
            final_completion_timeout: None,
        })
    }

    fn openai_engine_with_short_timeout(url: String) -> OpenAiRealtimeProtocolEngine {
        OpenAiRealtimeProtocolEngine::new(RealtimeEngineConfig {
            endpoint_display: url.clone(),
            url,
            auth_bearer: None,
            host_header: None,
            profile: OpenAiRealtimeProfile::OpenAi,
            turn_detection: TurnDetectionMode::ServerVad,
            languages: Vec::new(),
            final_completion_timeout: Some(Duration::from_millis(50)),
        })
    }

    async fn recv_json(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> Value {
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(text) = msg else {
            panic!("expected text message, got {msg:?}");
        };
        serde_json::from_str(&text).unwrap()
    }

    /// What the server does after the client's commit, in place of a
    /// post-commit completion.
    #[derive(Debug, Clone, Copy)]
    enum AfterCommit {
        /// Holds the connection open, so the client hits its post-commit
        /// grace timeout instead of a clean close.
        Hold,
        /// Closes the WebSocket cleanly.
        Close,
        /// Sends the empty-commit error, then waits for the close.
        EmptyCommitError,
    }

    // OpenAI server-VAD append-only profile: a transcript (append-only delta)
    // arrives BEFORE end-of-stream, then NO post-commit completion ever arrives.
    // Whatever the server does instead, the engine must finalize cleanly (Ok)
    // and preserve the already-typed text rather than failing into the
    // recovery path, and a held connection must finish via the SHORT grace
    // period instead of stalling for the full 30s FINAL_COMPLETION_TIMEOUT.
    #[tokio::test]
    async fn openai_post_commit_without_completion_keeps_the_typed_text() {
        for after_commit in [
            AfterCommit::Hold,
            AfterCommit::Close,
            AfterCommit::EmptyCommitError,
        ] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap_or_else(|e| panic!("{after_commit:?}: bind: {e}"));
            let addr = listener
                .local_addr()
                .unwrap_or_else(|e| panic!("{after_commit:?}: local addr: {e}"));
            let (delta_sent_tx, delta_sent_rx) = oneshot::channel::<()>();

            // A panic in here fails the row through `server.await` below, or
            // through the client's wait for `delta_sent_rx`.
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(stream).await.unwrap();

                let session = recv_json(&mut ws).await;
                assert_eq!(session["type"], "session.update", "{after_commit:?}");

                let append = recv_json(&mut ws).await;
                assert_eq!(
                    append["type"], "input_audio_buffer.append",
                    "{after_commit:?}"
                );

                // Server VAD finalizes the turn before recording stops: send the
                // append-only delta now, while audio input is still open.
                ws.send(Message::Text(
                    serde_json::json!({
                        "type": "conversation.item.input_audio_transcription.delta",
                        "item_id": "item-1",
                        "delta": "hello world"
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
                delta_sent_tx.send(()).ok();

                // After end-of-stream the client commits, but there is nothing
                // left to flush, so the server never sends a post-commit
                // completion.
                let commit = recv_json(&mut ws).await;
                assert_eq!(
                    commit["type"], "input_audio_buffer.commit",
                    "{after_commit:?}"
                );

                match after_commit {
                    AfterCommit::Hold => {}
                    AfterCommit::Close => {
                        ws.send(Message::Close(None)).await.ok();
                        return;
                    }
                    AfterCommit::EmptyCommitError => {
                        ws.send(Message::Text(
                            serde_json::json!({
                                "type": "error",
                                "error": { "message": "input_audio_buffer_commit_empty" }
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                    }
                }
                match ws.next().await {
                    Some(Ok(Message::Close(_))) | None => {}
                    other => panic!("{after_commit:?}: expected close or end, got {other:?}"),
                }
            });

            let engine = openai_engine_default_timeout(format!("ws://{addr}/realtime"));
            let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(8);
            let (text_tx, mut text_rx) = mpsc::channel::<String>(8);

            let request = test_request();
            let backend_task =
                tokio::spawn(
                    async move { engine.transcribe_stream(audio_rx, text_tx, &request).await },
                );

            audio_tx
                .send(vec![1; 160])
                .await
                .unwrap_or_else(|e| panic!("{after_commit:?}: send audio: {e}"));
            delta_sent_rx
                .await
                .unwrap_or_else(|e| panic!("{after_commit:?}: the server sent no delta: {e}"));

            // The append-only delta should reach the typing channel immediately.
            let emitted = timeout(Duration::from_secs(1), text_rx.recv())
                .await
                .unwrap_or_else(|e| panic!("{after_commit:?}: no text within 1 s: {e}"))
                .unwrap_or_else(|| panic!("{after_commit:?}: the text channel closed"));
            assert_eq!(emitted, "hello world", "{after_commit:?}");

            // Close input; the engine commits and then no post-commit
            // completion arrives.
            let finalize_start = Instant::now();
            drop(audio_tx);

            // Comfortably above POST_COMMIT_GRACE_AFTER_TEXT (2s) but far below
            // the 30s FINAL_COMPLETION_TIMEOUT, proving we use the short grace.
            let result = timeout(Duration::from_secs(5), backend_task)
                .await
                .unwrap_or_else(|_| {
                    panic!("{after_commit:?}: must not stall for the full final-completion timeout")
                })
                .unwrap_or_else(|e| panic!("{after_commit:?}: the backend task: {e}"));
            assert!(
                result.is_ok(),
                "{after_commit:?}: must finalize gracefully when text was already emitted, got {result:?}"
            );
            assert!(
                finalize_start.elapsed() < Duration::from_secs(10),
                "{after_commit:?}: finalize took too long; expected the short grace period, not the full timeout"
            );

            server
                .await
                .unwrap_or_else(|e| panic!("{after_commit:?}: the server: {e}"));
        }
    }

    // Guardrail: if NOTHING was ever emitted, a post-commit timeout must still
    // surface as an error so genuine failures trigger the recovery-save path.
    // This path keeps the full final-completion timeout (here overridden short).
    #[tokio::test]
    async fn openai_no_text_ever_emitted_still_errors_on_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            let session = recv_json(&mut ws).await;
            assert_eq!(session["type"], "session.update");

            let append = recv_json(&mut ws).await;
            assert_eq!(append["type"], "input_audio_buffer.append");

            let commit = recv_json(&mut ws).await;
            assert_eq!(commit["type"], "input_audio_buffer.commit");

            // Never send any transcript, just hold the socket open.
            match ws.next().await {
                Some(Ok(Message::Close(_))) | None => {}
                other => panic!("expected close or end, got {other:?}"),
            }
        });

        let engine = openai_engine_with_short_timeout(format!("ws://{addr}/realtime"));
        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(8);
        let (text_tx, _text_rx) = mpsc::channel::<String>(8);

        let request = test_request();
        audio_tx.send(vec![1; 160]).await.unwrap();
        drop(audio_tx);

        let result = timeout(
            Duration::from_secs(2),
            engine.transcribe_stream(audio_rx, text_tx, &request),
        )
        .await
        .unwrap();
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("timed out waiting for final transcription completion"),
            "expected a timeout error when no text was emitted, got: {err}"
        );

        server.await.unwrap();
    }

    // `transcribe` (command mode, agent turns) joins append deltas verbatim: a
    // word split across deltas stays whole, and the engine's item-boundary
    // space separates one item from the next.
    #[tokio::test]
    async fn openai_transcribe_joins_append_deltas_verbatim() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            assert_eq!(recv_json(&mut ws).await["type"], "session.update");
            assert_eq!(
                recv_json(&mut ws).await["type"],
                "input_audio_buffer.append"
            );
            assert_eq!(
                recv_json(&mut ws).await["type"],
                "input_audio_buffer.commit"
            );

            let deltas = [
                ("item-1", "Deep"),
                ("item-1", "gram"),
                ("item-1", " first"),
                ("item-1", "."),
                ("item-2", "Second"),
            ];
            for (item_id, delta) in deltas {
                ws.send(Message::Text(
                    serde_json::json!({
                        "type": "conversation.item.input_audio_transcription.delta",
                        "item_id": item_id,
                        "delta": delta
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            }
            ws.send(Message::Text(
                serde_json::json!({
                    "type": "conversation.item.input_audio_transcription.completed",
                    "item_id": "item-2",
                    "transcript": "Second"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            match ws.next().await {
                Some(Ok(Message::Close(_))) | None => {}
                other => panic!("expected close or end, got {other:?}"),
            }
        });

        let engine = openai_engine_default_timeout(format!("ws://{addr}/realtime"));
        let wav = crate::audio::wav::encode_wav(&[1; 160]).unwrap();
        let text = timeout(
            Duration::from_secs(5),
            engine.transcribe(&wav, &test_request()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(text, "Deepgram first. Second");

        server.await.unwrap();
    }
}
