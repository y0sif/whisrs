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
use super::{TranscriptionBackend, TranscriptionConfig};

/// External OpenAI-compatible realtime backend.
#[derive(Debug)]
pub struct OpenAiCompatibleRealtimeBackend {
    url: String,
    endpoint_display: String,
    auth_bearer: Option<String>,
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
        if profile != OpenAiRealtimeProfile::Lemonade {
            anyhow::bail!(
                "openai-compatible realtime backend currently supports only profile 'lemonade'"
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
            profile: OpenAiRealtimeProfile::Lemonade,
            turn_detection: self.turn_detection,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
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
            .contains("currently supports only profile 'lemonade'"));
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
            "ws://user:secret@localhost:1234/realtime?model=Whisper-Tiny".to_string(),
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

    #[tokio::test]
    async fn mock_ws_happy_path_emits_completed_phrases_and_commit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (delta_one_sent_tx, delta_one_sent_rx) = oneshot::channel::<()>();
        let (allow_first_completed_tx, allow_first_completed_rx) = oneshot::channel::<()>();
        let (delta_two_sent_tx, delta_two_sent_rx) = oneshot::channel::<()>();
        let (allow_second_completed_tx, allow_second_completed_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            let session = recv_json(&mut ws).await;
            assert_eq!(session["type"], "session.update");
            assert_eq!(session["session"]["model"], "Whisper-Tiny");
            assert_eq!(session["session"]["turn_detection"]["type"], "server_vad");

            let append_one = recv_json(&mut ws).await;
            assert_eq!(append_one["type"], "input_audio_buffer.append");
            assert!(append_one["audio"].as_str().is_some_and(|s| !s.is_empty()));

            ws.send(Message::Text(
                serde_json::json!({
                    "type": "conversation.item.input_audio_transcription.delta",
                    "item_id": "item-1",
                    "delta": "hel"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            delta_one_sent_tx.send(()).ok();
            allow_first_completed_rx.await.ok();

            let append_two = recv_json(&mut ws).await;
            assert_eq!(append_two["type"], "input_audio_buffer.append");
            assert!(append_two["audio"].as_str().is_some_and(|s| !s.is_empty()));

            ws.send(Message::Text(
                serde_json::json!({
                    "type": "conversation.item.input_audio_transcription.delta",
                    "item_id": "item-2",
                    "delta": "good"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            delta_two_sent_tx.send(()).ok();
            allow_second_completed_rx.await.ok();

            ws.send(Message::Text(
                serde_json::json!({
                    "type": "conversation.item.input_audio_transcription.completed",
                    "item_id": "item-1",
                    "transcript": "hello world"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            ws.send(Message::Text(
                serde_json::json!({
                    "type": "conversation.item.input_audio_transcription.completed",
                    "item_id": "item-2",
                    "transcript": "good morning"
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
                    "type": "conversation.item.input_audio_transcription.completed",
                    "item_id": "item-3",
                    "transcript": "tail flush"
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
        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(8);
        let (text_tx, mut text_rx) = mpsc::channel::<String>(8);

        let backend_task = tokio::spawn(async move {
            backend
                .transcribe_stream(audio_rx, text_tx, &test_request())
                .await
        });

        audio_tx.send(vec![1; 160]).await.unwrap();
        delta_one_sent_rx.await.unwrap();
        assert!(
            timeout(Duration::from_millis(100), text_rx.recv())
                .await
                .is_err(),
            "replaceable interim delta unexpectedly reached text channel early"
        );
        allow_first_completed_tx.send(()).ok();

        audio_tx.send(vec![2; 160]).await.unwrap();
        delta_two_sent_rx.await.unwrap();
        assert!(
            timeout(Duration::from_millis(100), text_rx.recv())
                .await
                .is_err(),
            "replaceable interim delta unexpectedly reached text channel early"
        );
        allow_second_completed_tx.send(()).ok();

        let first = timeout(Duration::from_secs(1), text_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, "hello world");

        let second = timeout(Duration::from_secs(1), text_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second, "good morning");

        drop(audio_tx);

        let third = timeout(Duration::from_secs(1), text_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(third, "tail flush");

        assert!(backend_task.await.unwrap().is_ok());
        server.await.unwrap();
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
}
