//! External OpenAI-compatible realtime transcription backend.
//!
//! This wrapper keeps provider-specific config validation at the repo boundary
//! while delegating the actual websocket protocol to the shared engine.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::audio::AudioChunk;

use super::openai_realtime_protocol::{
    OpenAiRealtimeProfile, OpenAiRealtimeProtocolEngine, RealtimeEngineConfig, TurnDetectionMode,
};
use super::{StreamText, TranscriptionBackend, TranscriptionConfig};

/// External OpenAI-compatible realtime backend.
#[derive(Debug)]
pub struct OpenAiCompatibleRealtimeBackend {
    url: String,
    endpoint_display: String,
    auth_bearer: Option<String>,
    profile: OpenAiRealtimeProfile,
    turn_detection: TurnDetectionMode,
    model: String,
    final_completion_timeout: Option<std::time::Duration>,
}

impl OpenAiCompatibleRealtimeBackend {
    /// Create a new external realtime backend from validated config values.
    pub fn new(
        url: String,
        model: String,
        profile: String,
        turn_detection: String,
        api_key: Option<String>,
    ) -> anyhow::Result<Self> {
        let url = url.trim().to_string();
        if url.is_empty() {
            anyhow::bail!("openai-compatible realtime URL cannot be empty");
        }

        let parsed_url = reqwest::Url::parse(&url)
            .map_err(|e| anyhow::anyhow!("invalid openai-compatible realtime URL: {e}"))?;
        match parsed_url.scheme() {
            "ws" | "wss" => {}
            scheme => {
                anyhow::bail!(
                    "openai-compatible realtime URL must use ws:// or wss://, got {scheme}://"
                );
            }
        }

        let model = model.trim().to_string();
        if model.is_empty() {
            anyhow::bail!("openai-compatible realtime model cannot be empty");
        }

        let profile = OpenAiRealtimeProfile::parse(profile.trim())?;
        if !matches!(
            profile,
            OpenAiRealtimeProfile::Lemonade | OpenAiRealtimeProfile::LlamaCpp
        ) {
            anyhow::bail!(
                "openai-compatible realtime backend supports only the 'lemonade' and 'llamacpp' profiles"
            );
        }

        let turn_detection = TurnDetectionMode::parse(turn_detection.trim())?;
        let auth_bearer = api_key
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty());

        Ok(Self {
            endpoint_display: sanitize_endpoint_display(&parsed_url),
            url,
            auth_bearer,
            profile,
            turn_detection,
            model,
            final_completion_timeout: None,
        })
    }

    // Keep the config-owned model as a fallback so direct unit tests or future
    // callers do not have to manually duplicate it into TranscriptionConfig.
    fn request_for_call(&self, request: &TranscriptionConfig) -> TranscriptionConfig {
        let mut request = request.clone();
        if request.model.trim().is_empty() {
            request.model = self.model.clone();
        }
        request
    }

    fn engine(&self) -> OpenAiRealtimeProtocolEngine {
        OpenAiRealtimeProtocolEngine::new(RealtimeEngineConfig {
            url: self.url.clone(),
            endpoint_display: self.endpoint_display.clone(),
            auth_bearer: self.auth_bearer.clone(),
            host_header: None,
            profile: self.profile,
            turn_detection: self.turn_detection,
            languages: Vec::new(),
            final_completion_timeout: self.final_completion_timeout,
        })
    }

    #[cfg(test)]
    fn with_final_completion_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.final_completion_timeout = Some(timeout);
        self
    }
}

fn sanitize_endpoint_display(url: &reqwest::Url) -> String {
    let mut sanitized = url.clone();
    let _ = sanitized.set_username("");
    let _ = sanitized.set_password(None);
    sanitized.set_query(None);
    sanitized.set_fragment(None);
    sanitized.to_string()
}

#[async_trait]
impl TranscriptionBackend for OpenAiCompatibleRealtimeBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        let request = self.request_for_call(config);
        self.engine().transcribe(audio, &request).await
    }

    async fn transcribe_stream(
        &self,
        audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        let request = self.request_for_call(config);
        self.engine()
            .transcribe_stream(audio_rx, text_tx, &request)
            .await
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    // Statically false for both profiles this backend accepts, exactly like
    // Deepgram. Neither wire format carries a prompt: `session_update` builds
    // `LemonadeSessionUpdate::new(model, turn_detection)` for `Lemonade`, and
    // `LlamaCppSessionUpdate::new`, which carries only the model, the turn
    // detection and the input rate, for `LlamaCpp`. The engine passes
    // `request.prompt` down, but both drop it on the floor, so a non-empty
    // `[general] vocabulary` here would otherwise get real transcripts
    // discarded as echoes of a prompt that was never sent.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        false
    }

    // Whatever the configured profile's delta mode implies: `lemonade` sends
    // completed utterances, `llamacpp` sends append-only fragments.
    fn stream_text(&self) -> StreamText {
        self.profile.stream_text()
    }

    // Live-only: a recording needs a batch backend.
    async fn transcribe_segments(
        &self,
        _audio: &[u8],
        _config: &TranscriptionConfig,
    ) -> anyhow::Result<super::Transcript> {
        Err(super::live_only_error("openai-compatible-realtime"))
    }

    // Never chunked: `transcribe_segments` always errors.
    fn max_segment_chunk_secs(&self, _config: &TranscriptionConfig) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tokio::time::{sleep, timeout, Duration};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    #[test]
    fn rejects_empty_url() {
        let err = OpenAiCompatibleRealtimeBackend::new(
            String::new(),
            "Whisper-Tiny".to_string(),
            "lemonade".to_string(),
            "server-vad".to_string(),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("URL cannot be empty"));
    }

    #[test]
    fn rejects_non_websocket_url() {
        let err = OpenAiCompatibleRealtimeBackend::new(
            "http://localhost:1234/realtime".to_string(),
            "Whisper-Tiny".to_string(),
            "lemonade".to_string(),
            "server-vad".to_string(),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("ws:// or wss://"));
    }

    #[test]
    fn rejects_empty_model() {
        let err = OpenAiCompatibleRealtimeBackend::new(
            "ws://localhost:1234/realtime".to_string(),
            "  ".to_string(),
            "lemonade".to_string(),
            "server-vad".to_string(),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("model cannot be empty"));
    }

    #[test]
    fn rejects_unsupported_profile() {
        let err = OpenAiCompatibleRealtimeBackend::new(
            "ws://localhost:1234/realtime".to_string(),
            "Whisper-Tiny".to_string(),
            "openai".to_string(),
            "server-vad".to_string(),
            None,
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("supports only the 'lemonade' and 'llamacpp' profiles"));
    }

    /// `lemonade` forwards completed utterances, so queued ones need a space
    /// between them; `llamacpp` forwards append deltas as they arrive. Neither
    /// wire format has a prompt field, so the echo filter stays off for both.
    #[test]
    fn accepted_profiles_pick_stream_text_and_send_no_prompt() {
        for (profile, expected, stream_text) in [
            (
                "lemonade",
                OpenAiRealtimeProfile::Lemonade,
                StreamText::Phrases,
            ),
            (
                "llamacpp",
                OpenAiRealtimeProfile::LlamaCpp,
                StreamText::Fragments,
            ),
        ] {
            let backend = OpenAiCompatibleRealtimeBackend::new(
                "ws://localhost:1234/realtime".to_string(),
                "Whisper-Tiny".to_string(),
                profile.to_string(),
                "server-vad".to_string(),
                None,
            )
            .unwrap_or_else(|e| panic!("{profile}: {e}"));

            assert_eq!(backend.profile, expected, "{profile}: profile");
            assert_eq!(backend.stream_text(), stream_text, "{profile}: stream text");
            assert!(
                !backend.sends_prompt(&test_request()),
                "{profile}: sends no prompt"
            );
        }
    }

    #[test]
    fn rejects_unsupported_turn_detection() {
        let err = OpenAiCompatibleRealtimeBackend::new(
            "ws://localhost:1234/realtime".to_string(),
            "Whisper-Tiny".to_string(),
            "lemonade".to_string(),
            "bogus".to_string(),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("unsupported turn detection"));
    }

    #[test]
    fn trims_optional_bearer_and_sanitizes_endpoint_display() {
        let backend = OpenAiCompatibleRealtimeBackend::new(
            "ws://user:secret@localhost:1234/realtime".to_string(),
            "Whisper-Tiny".to_string(),
            "lemonade".to_string(),
            "manual-commit".to_string(),
            Some("  test-token  ".to_string()),
        )
        .unwrap();

        assert_eq!(backend.auth_bearer.as_deref(), Some("test-token"));
        assert_eq!(
            backend.endpoint_display,
            "ws://localhost:1234/realtime".to_string()
        );
    }

    #[test]
    fn falls_back_to_configured_model_when_request_model_is_blank() {
        let backend = OpenAiCompatibleRealtimeBackend::new(
            "ws://localhost:1234/realtime".to_string(),
            "Whisper-Tiny".to_string(),
            "lemonade".to_string(),
            "server-vad".to_string(),
            None,
        )
        .unwrap();

        let request = backend.request_for_call(&TranscriptionConfig {
            language: "auto".to_string(),
            model: "   ".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        });

        assert_eq!(request.model, "Whisper-Tiny");
    }

    #[test]
    fn supports_streaming_is_true() {
        let backend = OpenAiCompatibleRealtimeBackend::new(
            "ws://localhost:1234/realtime".to_string(),
            "Whisper-Tiny".to_string(),
            "lemonade".to_string(),
            "server-vad".to_string(),
            None,
        )
        .unwrap();

        assert!(backend.supports_streaming());
    }

    #[test]
    fn accepts_manual_commit_turn_detection() {
        let backend = OpenAiCompatibleRealtimeBackend::new(
            "ws://localhost:1234/realtime".to_string(),
            "Whisper-Tiny".to_string(),
            "lemonade".to_string(),
            "manual-commit".to_string(),
            None,
        )
        .unwrap();

        assert_eq!(backend.turn_detection, TurnDetectionMode::ManualCommit);
    }

    fn test_request() -> TranscriptionConfig {
        TranscriptionConfig {
            language: "auto".to_string(),
            model: "Whisper-Tiny".to_string(),
            prompt: None,
            keyterms: Vec::new(),
            diarize: false,
            vad: Default::default(),
            soft_voices: true,
        }
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

    fn delta(item_id: &str, delta: &str) -> Value {
        serde_json::json!({
            "type": "conversation.item.input_audio_transcription.delta",
            "item_id": item_id,
            "delta": delta
        })
    }

    fn completed(item_id: &str, transcript: &str) -> Value {
        serde_json::json!({
            "type": "conversation.item.input_audio_transcription.completed",
            "item_id": item_id,
            "transcript": transcript
        })
    }

    struct StreamCase {
        profile: &'static str,
        /// Server events sent while the audio is still streaming.
        live: Vec<Value>,
        typed_live: &'static [&'static str],
        /// Server events sent in reply to the end-of-stream commit.
        after_commit: Vec<Value>,
        /// How long the server stays quiet after the first of those events.
        quiet_after_first: Duration,
        /// The server closes the socket after those events, as llama.cpp does.
        closes: bool,
        typed_tail: &'static [&'static str],
        session_rate: Option<u32>,
    }

    #[tokio::test]
    async fn mock_ws_happy_path_types_live_text_and_the_tail_after_commit() {
        let cases = [
            // Interim deltas are never typed, completed phrases are.
            StreamCase {
                profile: "lemonade",
                live: vec![
                    delta("item-1", "hel"),
                    completed("item-1", "hello world"),
                    delta("item-2", "good"),
                    completed("item-2", "good morning"),
                ],
                typed_live: &["hello world", "good morning"],
                after_commit: vec![completed("item-3", "tail flush")],
                quiet_after_first: Duration::ZERO,
                closes: false,
                typed_tail: &["tail flush"],
                session_rate: None,
            },
            // Deltas are typed as they arrive. The commit is acknowledged, the
            // server goes quiet for longer than the 2 s grace (a server slower
            // than real time working through a pause in the speech), then the
            // word it still held arrives as one more delta of the same item,
            // and its completed adds nothing.
            StreamCase {
                profile: "llamacpp",
                live: vec![delta("item_1", " Hello"), delta("item_1", " there")],
                typed_live: &[" Hello", " there"],
                after_commit: vec![
                    serde_json::json!({
                        "type": "input_audio_buffer.committed",
                        "item_id": "item_1"
                    }),
                    delta("item_1", " friend"),
                    completed("item_1", " Hello there friend"),
                ],
                quiet_after_first: Duration::from_millis(2500),
                closes: true,
                typed_tail: &[" friend"],
                session_rate: Some(16_000),
            },
        ];

        for case in cases {
            let name = case.profile;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (live, after_commit, quiet, closes) = (
                case.live,
                case.after_commit,
                case.quiet_after_first,
                case.closes,
            );
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(stream).await.unwrap();

                let session = recv_json(&mut ws).await;
                let append = recv_json(&mut ws).await;
                assert_eq!(append["type"], "input_audio_buffer.append", "{name}");
                for event in live {
                    ws.send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }

                let commit = recv_json(&mut ws).await;
                for (i, event) in after_commit.into_iter().enumerate() {
                    ws.send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                    if i == 0 {
                        sleep(quiet).await;
                    }
                }
                if closes {
                    ws.send(Message::Close(None)).await.unwrap();
                }
                match ws.next().await {
                    Some(Ok(Message::Close(_))) | None => {}
                    other => panic!("{name}: expected the client's close, got {other:?}"),
                }
                (session, commit)
            });

            let backend = OpenAiCompatibleRealtimeBackend::new(
                format!("ws://{addr}/realtime"),
                "Whisper-Tiny".to_string(),
                name.to_string(),
                "server-vad".to_string(),
                None,
            )
            .unwrap();
            let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(8);
            let (text_tx, mut text_rx) = mpsc::channel::<String>(8);
            let backend_task = tokio::spawn(async move {
                backend
                    .transcribe_stream(audio_rx, text_tx, &test_request())
                    .await
            });

            audio_tx.send(vec![1; 160]).await.unwrap();
            for expected in case.typed_live {
                let typed = timeout(Duration::from_secs(1), text_rx.recv())
                    .await
                    .unwrap_or_else(|_| panic!("{name}: {expected:?} not typed while streaming"));
                assert_eq!(typed.as_deref(), Some(*expected), "{name}: while streaming");
            }

            drop(audio_tx);
            let mut tail = Vec::new();
            while let Some(text) = timeout(Duration::from_secs(5), text_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{name}: the stream did not end"))
            {
                tail.push(text);
            }
            assert_eq!(tail, case.typed_tail, "{name}: typed after the stop");
            let result = backend_task.await.unwrap();
            assert!(result.is_ok(), "{name}: {result:?}");

            let (session, commit) = server.await.unwrap();
            assert_eq!(session["type"], "session.update", "{name}");
            assert_eq!(session["session"]["model"], "Whisper-Tiny", "{name}");
            assert_eq!(
                session["session"]["turn_detection"]["type"], "server_vad",
                "{name}"
            );
            assert_eq!(
                session["session"]["audio"]["input"]["format"].get("rate"),
                case.session_rate.map(Value::from).as_ref(),
                "{name}: session input rate"
            );
            assert_eq!(
                commit,
                serde_json::json!({ "type": "input_audio_buffer.commit" }),
                "{name}: the end-of-stream commit"
            );
        }
    }

    #[tokio::test]
    async fn mock_ws_error_event_is_returned() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            let session = recv_json(&mut ws).await;
            assert_eq!(session["type"], "session.update");

            let append = recv_json(&mut ws).await;
            assert_eq!(append["type"], "input_audio_buffer.append");

            ws.send(Message::Text(
                serde_json::json!({
                    "type": "error",
                    "error": { "message": "bad auth" }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            sleep(Duration::from_millis(100)).await;
        });

        let backend = OpenAiCompatibleRealtimeBackend::new(
            format!("ws://{addr}/realtime"),
            "Whisper-Tiny".to_string(),
            "lemonade".to_string(),
            "server-vad".to_string(),
            None,
        )
        .unwrap();
        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(4);
        let (text_tx, _text_rx) = mpsc::channel::<String>(4);

        let backend_task = tokio::spawn(async move {
            backend
                .transcribe_stream(audio_rx, text_tx, &test_request())
                .await
        });
        audio_tx.send(vec![4; 160]).await.unwrap();
        drop(audio_tx);

        let err = backend_task.await.unwrap().unwrap_err();
        assert!(
            err.to_string().contains("bad auth"),
            "unexpected error: {err}"
        );

        server.await.unwrap();
    }

    #[tokio::test]
    async fn mock_ws_timeout_after_commit_uses_short_test_timeout() {
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

            sleep(Duration::from_millis(250)).await;
        });

        let backend = OpenAiCompatibleRealtimeBackend::new(
            format!("ws://{addr}/realtime"),
            "Whisper-Tiny".to_string(),
            "lemonade".to_string(),
            "server-vad".to_string(),
            None,
        )
        .unwrap()
        .with_final_completion_timeout(Duration::from_millis(50));
        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(4);
        let (text_tx, _text_rx) = mpsc::channel::<String>(4);

        audio_tx.send(vec![3; 160]).await.unwrap();
        drop(audio_tx);

        let err = backend
            .transcribe_stream(audio_rx, text_tx, &test_request())
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("timed out waiting for final transcription completion"),
            "unexpected timeout-path error: {err}"
        );

        server.await.unwrap();
    }

    #[tokio::test]
    async fn mock_ws_buffer_cleared_after_commit_finishes_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            let session = recv_json(&mut ws).await;
            assert_eq!(session["type"], "session.update");

            let append = recv_json(&mut ws).await;
            assert_eq!(append["type"], "input_audio_buffer.append");

            ws.send(Message::Text(
                serde_json::json!({
                    "type": "conversation.item.input_audio_transcription.completed",
                    "transcript": "hello there"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let commit = recv_json(&mut ws).await;
            assert_eq!(commit["type"], "input_audio_buffer.commit");

            ws.send(Message::Text(
                serde_json::json!({
                    "type": "input_audio_buffer.cleared"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            match ws.next().await.unwrap().unwrap() {
                Message::Close(_) => {}
                other => panic!("expected close frame, got {other:?}"),
            }
        });

        let backend = OpenAiCompatibleRealtimeBackend::new(
            format!("ws://{addr}/realtime"),
            "Whisper-Tiny".to_string(),
            "lemonade".to_string(),
            "server-vad".to_string(),
            None,
        )
        .unwrap();
        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(4);
        let (text_tx, mut text_rx) = mpsc::channel::<String>(4);

        let backend_task = tokio::spawn(async move {
            backend
                .transcribe_stream(audio_rx, text_tx, &test_request())
                .await
        });

        audio_tx.send(vec![8; 160]).await.unwrap();
        let first = timeout(Duration::from_secs(1), text_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, "hello there");

        drop(audio_tx);

        assert!(backend_task.await.unwrap().is_ok());
        server.await.unwrap();
    }
}
