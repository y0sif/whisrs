use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;
use tracing::{debug, error, info, warn};

use crate::audio::AudioChunk;
use crate::transcription::{TranscriptionBackend, TranscriptionConfig};

use super::profile::{DeltaMode, OpenAiRealtimeProfile, TurnDetectionMode};
use super::wire::{AudioBufferAppend, AudioBufferCommit, ServerMessage};
use super::{encode_pcm_base64, resample_16k_to_24k};

const FINAL_COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Immutable connection options for the shared realtime engine.
#[derive(Debug, Clone)]
pub struct RealtimeEngineConfig {
    pub url: String,
    pub endpoint_display: String,
    pub auth_bearer: Option<String>,
    pub host_header: Option<String>,
    pub profile: OpenAiRealtimeProfile,
    pub turn_detection: TurnDetectionMode,
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
    // running copy so a later completed event cannot re-emit the same content.
    emitted_append_text: String,
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
            // preserves the text stream exactly as emitted by the provider.
            DeltaMode::AppendOnly => {
                self.emitted_append_text.push_str(&delta);
                DeltaAction {
                    emit_text: Some(delta),
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

        let collector = async {
            let mut full_text = String::new();
            while let Some(text) = text_rx.recv().await {
                if !full_text.is_empty() {
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
                    let msg_result =
                        tokio::time::timeout(self.final_completion_timeout(), ws_source.next())
                            .await
                            .map_err(|_| {
                                anyhow::anyhow!(
                                    "timed out waiting for final transcription completion from {} after commit",
                                    self.config.endpoint_display
                                )
                            })?;

                    let Some(msg_result) = msg_result else {
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

// This trait impl keeps the provider wrappers thin today, but the engine is
// still an internal protocol component rather than a user-facing backend.
// If TranscriptionBackend grows more wrapper-level surface area in the future,
// consider removing this impl and keeping the trait boundary only on the
// concrete wrapper structs.
#[async_trait]
impl TranscriptionBackend for OpenAiRealtimeProtocolEngine {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        OpenAiRealtimeProtocolEngine::transcribe(self, audio, config).await
    }

    async fn transcribe_stream(
        &self,
        audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        OpenAiRealtimeProtocolEngine::transcribe_stream(self, audio_rx, text_tx, config).await
    }

    fn supports_streaming(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_before_input_close_does_not_finalize_stream() {
        let mut state = StreamState::default();
        let action = state.on_completed(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            Some("hello".to_string()),
        );

        assert_eq!(action.emit_text.as_deref(), Some("hello"));
        assert!(!action.finalizes_stream);
        assert!(!state.final_completion_seen);
    }

    #[test]
    fn post_commit_completed_finalizes_stream() {
        let mut state = StreamState::default();
        state.note_input_closed();
        state.note_commit_sent();

        let action = state.on_completed(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            Some("hello".to_string()),
        );

        assert!(action.finalizes_stream);
        assert!(state.final_completion_seen);
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

    #[test]
    fn completed_clears_latest_interim_state() {
        let mut state = StreamState::default();
        state.on_delta(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            "hello wor".to_string(),
        );

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

    #[test]
    fn append_only_delta_emits_text_and_tracks_emitted_content() {
        let mut state = StreamState::default();

        let action = state.on_delta(
            OpenAiRealtimeProfile::OpenAi,
            Some("item-1"),
            "hello".to_string(),
        );

        assert_eq!(action.emit_text.as_deref(), Some("hello"));
        assert_eq!(state.emitted_append_text, "hello");
    }

    #[test]
    fn lemonade_completed_is_emitted_only_once_per_item_id() {
        let mut state = StreamState::default();

        let first = state.on_completed(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            Some("hello world".to_string()),
        );
        let second = state.on_completed(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            Some("hello world".to_string()),
        );

        assert_eq!(first.emit_text.as_deref(), Some("hello world"));
        assert!(second.emit_text.is_none());
    }

    #[test]
    fn lemonade_completed_is_suppressed_by_trimmed_text_without_item_id() {
        let mut state = StreamState::default();

        let first = state.on_completed(
            OpenAiRealtimeProfile::Lemonade,
            None,
            Some("hello world".to_string()),
        );
        let second = state.on_completed(
            OpenAiRealtimeProfile::Lemonade,
            None,
            Some("  hello world  ".to_string()),
        );

        assert_eq!(first.emit_text.as_deref(), Some("hello world"));
        assert!(second.emit_text.is_none());
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

    #[test]
    fn lemonade_partial_sequence_emits_nothing_until_completed() {
        let mut state = StreamState::default();

        let first = state.on_delta(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            "hel".to_string(),
        );
        let second = state.on_delta(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            "hello wor".to_string(),
        );
        let third = state.on_delta(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            "hello world".to_string(),
        );
        let completed = state.on_completed(
            OpenAiRealtimeProfile::Lemonade,
            Some("item-1"),
            Some("hello world".to_string()),
        );

        assert!(first.emit_text.is_none());
        assert!(second.emit_text.is_none());
        assert!(third.emit_text.is_none());
        assert_eq!(completed.emit_text.as_deref(), Some("hello world"));
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
