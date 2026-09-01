//! Generic HTTP ASR sidecar transcription backend.
//!
//! This backend keeps the Rust daemon independent from Python/PyTorch by
//! sending WAV audio to a local HTTP sidecar.

use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;
use tracing::{debug, warn};

use super::{TranscriptionBackend, TranscriptionConfig};

/// Keep a guardrail so a runaway recording does not create an unbounded
/// multipart request.
const MAX_FILE_SIZE: usize = 1024 * 1024 * 1024;

/// Generic HTTP ASR sidecar transcription backend.
pub struct AsrSidecarBackend {
    client: reqwest::Client,
    url: String,
    api_key: Option<String>,
}

impl AsrSidecarBackend {
    /// Create a new sidecar backend with the transcription URL and optional API key.
    pub fn new(url: String, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
            api_key: api_key
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty()),
        }
    }

    /// The endpoint this backend posts to, exactly as configured.
    ///
    /// Test seam. The backend factory is the only place a URL and a key are
    /// joined into a backend, and it lives in the `whisrsd` binary crate, so
    /// neither `#[cfg(test)]` nor `pub(crate)` here is visible to the test that
    /// proves that wiring. Hidden from the docs to keep it off the supported
    /// surface.
    #[doc(hidden)]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The normalized API key, or `None` for an unauthenticated sidecar.
    /// Test seam; see [`AsrSidecarBackend::url`].
    #[doc(hidden)]
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

/// The URL actually posted to, with trailing path slashes removed.
///
/// Some OpenAI-compatible endpoints 307-redirect when the URL has a trailing
/// slash, which can downgrade http→https and cause reqwest to abort multipart
/// POSTs. Trimming makes both forms work without a redirect. A URL made only of
/// slashes would trim to the empty string, which reqwest rejects with an opaque
/// relative-URL error, so keep the original in that case and let the request
/// fail against what the user actually configured.
///
/// Only the *path* carries that noise. A trailing slash inside a query value or
/// a fragment is data the user typed, so a URL carrying `?` or `#` is posted
/// exactly as configured rather than silently rewritten.
fn effective_url(url: &str) -> &str {
    if url.contains('?') || url.contains('#') {
        return url;
    }
    let trimmed = url.trim_end_matches('/');
    if trimmed.is_empty() {
        url
    } else {
        trimmed
    }
}

/// Response from the ASR sidecar.
#[derive(Debug, Deserialize)]
pub struct AsrSidecarResponse {
    /// Plain text transcript. Sidecars may also return richer diarized output,
    /// but whisrs currently consumes the flattened text for typing.
    pub text: String,
}

#[derive(Debug, Deserialize)]
struct AsrSidecarErrorResponse {
    error: Option<String>,
    detail: Option<serde_json::Value>,
}

impl AsrSidecarErrorResponse {
    fn message(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        match &self.detail {
            Some(serde_json::Value::String(detail)) => detail.clone(),
            Some(detail) => detail.to_string(),
            None => "unknown sidecar error".to_string(),
        }
    }
}

#[async_trait]
impl TranscriptionBackend for AsrSidecarBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        if audio.len() > MAX_FILE_SIZE {
            anyhow::bail!(
                "audio file too large ({} bytes, max {} bytes / 1GB)",
                audio.len(),
                MAX_FILE_SIZE
            );
        }

        if audio.is_empty() {
            anyhow::bail!("cannot transcribe empty audio");
        }

        if self.url.trim().is_empty() {
            anyhow::bail!("no ASR sidecar URL configured");
        }

        debug!(
            "sending {} bytes to ASR sidecar (model={}, language={})",
            audio.len(),
            config.model,
            config.language
        );

        let file_part = multipart::Part::bytes(audio.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")?;

        let mut form = multipart::Form::new()
            .part("file", file_part)
            .text("model", config.model.clone());

        if config.language != "auto" {
            form = form.text("language", config.language.clone());
        }
        if let Some(prompt) = &config.prompt {
            form = form.text("hotwords", prompt.clone());
        }

        let url = effective_url(&self.url);
        if url != self.url {
            debug!("trimmed trailing slashes from ASR sidecar URL: {url}");
        }
        let mut request = self.client.post(url);
        if let Some(key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        let response = request.multipart(form).send().await?;
        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            if let Ok(err_resp) = serde_json::from_str::<AsrSidecarErrorResponse>(&body) {
                anyhow::bail!(
                    "ASR sidecar error ({}): {}",
                    status.as_u16(),
                    err_resp.message()
                );
            }
            anyhow::bail!("ASR sidecar error ({}): {}", status.as_u16(), body);
        }

        let parsed: AsrSidecarResponse = serde_json::from_str(&body)?;
        let text = parsed.text.trim().to_string();

        if text.is_empty() {
            warn!("ASR sidecar returned empty transcription");
        }

        Ok(text)
    }

    // Uses the default transcribe_stream (collect + transcribe). Model-specific
    // streaming behavior belongs in the sidecar process.

    // The prompt does reach the sidecar, just under a different name: it is
    // sent as the `hotwords` multipart field.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn test_config() -> TranscriptionConfig {
        TranscriptionConfig {
            language: "en".to_string(),
            model: "test-asr-model".to_string(),
            prompt: None,
            keyterms: Vec::new(),
        }
    }

    /// One-shot HTTP server: accepts a single connection, reads the whole
    /// request, answers `200 OK` with `body`, and hands back the request head
    /// so a test can assert on what was actually put on the wire.
    async fn serve_once(listener: TcpListener, body: &'static str) -> String {
        let (mut stream, _) = listener.accept().await.unwrap();

        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed before sending a request head");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();

        // Drain the multipart body first: replying while the client is still
        // writing surfaces as a connection reset instead of a parsed response.
        let content_length: usize = head
            .to_ascii_lowercase()
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .expect(
                "reqwest must send content-length for a multipart body with known part lengths",
            );
        while buf.len() < head_end + content_length {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }

        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        head
    }

    #[tokio::test]
    async fn transcribe_sends_bearer_authorization_header() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(listener, r#"{"text": "ok"}"#));

        let backend = AsrSidecarBackend::new(
            format!("http://{addr}/transcribe"),
            Some("sk-test-key".to_string()),
        );
        let text = backend
            .transcribe(&[1, 2, 3], &test_config())
            .await
            .unwrap();
        assert_eq!(text, "ok");

        // reqwest lowercases header names on the wire, so compare lowercased.
        let head = server.await.unwrap().to_ascii_lowercase();
        assert!(
            head.contains("authorization: bearer sk-test-key"),
            "request head is missing the bearer token: {head}"
        );
    }

    #[tokio::test]
    async fn transcribe_sends_no_authorization_header_without_key() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(listener, r#"{"text": "ok"}"#));

        let backend = AsrSidecarBackend::new(format!("http://{addr}/transcribe"), None);
        let text = backend
            .transcribe(&[1, 2, 3], &test_config())
            .await
            .unwrap();
        assert_eq!(text, "ok");

        let head = server.await.unwrap().to_ascii_lowercase();
        assert!(
            !head.contains("authorization:"),
            "unauthenticated request must not carry an authorization header: {head}"
        );
    }

    #[tokio::test]
    async fn transcribe_posts_to_the_trimmed_path() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(listener, r#"{"text": "ok"}"#));

        // A trailing slash makes some OpenAI-compatible servers 307-redirect,
        // which aborts the multipart POST; the request must go out trimmed.
        let backend = AsrSidecarBackend::new(format!("http://{addr}/transcribe/"), None);
        let text = backend
            .transcribe(&[1, 2, 3], &test_config())
            .await
            .unwrap();
        assert_eq!(text, "ok");

        let head = server.await.unwrap();
        assert!(
            head.starts_with("POST /transcribe HTTP/1.1\r\n"),
            "expected the trailing slash to be trimmed off the request line: {head}"
        );
    }

    #[test]
    fn effective_url_leaves_a_plain_url_alone() {
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe"),
            "http://127.0.0.1:8765/transcribe"
        );
    }

    #[test]
    fn effective_url_trims_one_trailing_slash() {
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe/"),
            "http://127.0.0.1:8765/transcribe"
        );
    }

    #[test]
    fn effective_url_trims_multiple_trailing_slashes() {
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe///"),
            "http://127.0.0.1:8765/transcribe"
        );
    }

    #[test]
    fn effective_url_keeps_original_when_trimming_would_empty_it() {
        // Trimming "/" to "" passes the empty-URL guard (which checks the
        // configured value) and then dies inside reqwest with an opaque
        // relative-URL error. Keep what the user configured instead.
        assert_eq!(effective_url("/"), "/");
        assert_eq!(effective_url("///"), "///");
    }

    #[test]
    fn effective_url_leaves_a_query_alone() {
        // The trailing slash belongs to the query value, not the path, so
        // trimming it would post a different `x=` than the user configured.
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe?x=1/"),
            "http://127.0.0.1:8765/transcribe?x=1/"
        );
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe/?x=1"),
            "http://127.0.0.1:8765/transcribe/?x=1"
        );
    }

    #[test]
    fn effective_url_leaves_a_fragment_alone() {
        assert_eq!(
            effective_url("http://127.0.0.1:8765/transcribe#frag/"),
            "http://127.0.0.1:8765/transcribe#frag/"
        );
    }

    #[tokio::test]
    async fn transcribe_rejects_empty_audio() {
        let backend = AsrSidecarBackend::new("http://127.0.0.1:8765/transcribe".to_string(), None);
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "test-asr-model".to_string(),
            prompt: None,
            keyterms: Vec::new(),
        };
        let err = backend.transcribe(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("empty audio"));
    }

    #[tokio::test]
    async fn transcribe_rejects_missing_url() {
        let backend = AsrSidecarBackend::new(String::new(), None);
        let config = TranscriptionConfig {
            language: "en".to_string(),
            model: "test-asr-model".to_string(),
            prompt: None,
            keyterms: Vec::new(),
        };
        let err = backend.transcribe(&[1, 2, 3], &config).await.unwrap_err();
        assert!(err.to_string().contains("sidecar URL"));
    }

    #[test]
    fn empty_api_key_is_normalized_to_none() {
        let backend = AsrSidecarBackend::new(
            "http://127.0.0.1:8765/transcribe".to_string(),
            Some(String::new()),
        );
        assert!(backend.api_key.is_none());
    }

    #[test]
    fn api_key_is_stored_when_present() {
        let backend = AsrSidecarBackend::new(
            "http://127.0.0.1:8765/transcribe".to_string(),
            Some("sk-test-key".to_string()),
        );
        assert_eq!(backend.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn whitespace_only_api_key_is_normalized_to_none() {
        let backend = AsrSidecarBackend::new(
            "http://127.0.0.1:8765/transcribe".to_string(),
            Some("   ".to_string()),
        );
        assert!(backend.api_key.is_none());
    }

    #[test]
    fn api_key_whitespace_is_trimmed() {
        let backend = AsrSidecarBackend::new(
            "http://127.0.0.1:8765/transcribe".to_string(),
            Some("  sk-test-key  ".to_string()),
        );
        assert_eq!(backend.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn parse_asr_sidecar_response() {
        let body = r#"{"text": "Hello world"}"#;
        let parsed: AsrSidecarResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.text, "Hello world");
    }

    #[test]
    fn parse_asr_sidecar_error() {
        let body = r#"{"error": "model failed to load"}"#;
        let parsed: AsrSidecarErrorResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.message(), "model failed to load");
    }

    #[test]
    fn parse_fastapi_error_detail() {
        let body = r#"{"detail": "request asked for wrong model"}"#;
        let parsed: AsrSidecarErrorResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.message(), "request asked for wrong model");
    }
}
