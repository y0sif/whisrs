//! Deepgram transcription backend (REST + streaming WebSocket).
//!
//! Supports two modes:
//! - **REST**: POST WAV audio to `/v1/listen` (non-streaming, simple).
//! - **Streaming**: WebSocket to `wss://api.deepgram.com/v1/listen` with raw
//!   PCM binary frames and real-time transcription results.

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;
use tracing::{debug, error, info, warn};

use crate::audio::AudioChunk;

use super::{TranscriptionBackend, TranscriptionConfig};

/// Deepgram REST API endpoint for pre-recorded audio.
const DEEPGRAM_REST_URL: &str = "https://api.deepgram.com/v1/listen";

/// Deepgram WebSocket endpoint for live streaming.
const DEEPGRAM_WS_URL: &str = "wss://api.deepgram.com/v1/listen";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Resolve the API key from the struct field or `WHISRS_DEEPGRAM_API_KEY`.
fn resolve_api_key(api_key: &str) -> anyhow::Result<String> {
    if !api_key.is_empty() {
        return Ok(api_key.to_string());
    }
    std::env::var("WHISRS_DEEPGRAM_API_KEY").map_err(|_| {
        anyhow::anyhow!(
            "no Deepgram API key configured — set WHISRS_DEEPGRAM_API_KEY or add [deepgram] to config.toml"
        )
    })
}

/// Map whisrs language codes to Deepgram's `language` query parameter.
/// "auto" maps to "multi" (Deepgram's auto-detect / code-switching mode).
fn map_language(language: &str) -> &str {
    if language == "auto" {
        "multi"
    } else {
        language
    }
}

/// Percent-encode one query-string value.
///
/// The streaming URL below is assembled by hand and there is no encoder in the
/// dependency tree, so this has to exist: Deepgram rejects the handshake with a
/// bare 400 when a multi-word keyterm leaves a raw space in the URI.
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Assemble a query string from `params`, percent-encoding every value.
///
/// Extracted from `transcribe_stream` so the encoding is testable: reqwest does
/// this for the REST path, but the WebSocket URI is built by hand and a raw
/// space in a multi-word keyterm makes it invalid.
fn build_query_string(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Byte budget for the `keyterm` portion of a Deepgram request.
///
/// Every parameter rides in the URI — the request line for REST, the handshake
/// target for the WebSocket — and proxies and Deepgram's own edge typically cap
/// a request line around 8KB. Past that the edge answers a bare `400` that
/// never mentions the vocabulary, so streaming dictation just stops working.
/// 4096 leaves ample room for the endpoint, the fixed parameters and any
/// forward proxy's own additions.
///
/// This bounds **bytes and nothing else**. It does not imply a bound on the
/// number of terms, and it does not imply a bound on tokens: `"ab cd ef"` × 400
/// stops here at 195 terms / 4095 bytes while carrying at least 585
/// whitespace-delimited words. [`KEYTERM_MAX_TERMS`] and [`KEYTERM_MAX_WORDS`]
/// are what bound those.
pub(crate) const KEYTERM_QUERY_BUDGET_BYTES: usize = 4096;

/// Ceiling on how many `keyterm` params ride in one request.
///
/// This bounds **the parameter count and nothing else**: it keeps a vocabulary
/// of very short terms from turning one request into thousands of query params,
/// which the byte budget alone permits (single-char terms cost 10 bytes each,
/// so 4096 bytes is ~409 params). It is deliberately *not* a token bound — an
/// earlier revision of this comment claimed staying under 200 terms kept a
/// byte-legal vocabulary inside Deepgram's 500-token cap, and that is false:
/// the byte budget cuts `"ab cd ef"` × 400 off at 195 terms, below this cap,
/// with ≥585 words on the wire. [`KEYTERM_MAX_WORDS`] is the token bound.
pub(crate) const KEYTERM_MAX_TERMS: usize = 200;

/// Ceiling on the total whitespace-delimited word count across all `keyterm`s.
///
/// Deepgram caps the keyterm set at 500 **tokens** per request, not 500 terms,
/// and whisrs cannot tokenize client-side. Neither limit above bounds tokens.
/// Measured against the two gates as they stood: `"ab cd ef"` × 400 → 195 terms
/// at 4095 bytes and ≥585 words; `"a b c d e"` × 400 → 157 terms at 4082 bytes
/// and ≥785 words. So a ~136-term two-word domain glossary ("atrial
/// fibrillation") passes both, lands at roughly 550-800 tokens, and earns
/// exactly the opaque `400` these limits exist to prevent — after
/// `Config::validate` has already told the user how many terms reach Deepgram,
/// when in fact none do, because the whole request is rejected.
///
/// A tokenizer never emits fewer tokens than there are whitespace-delimited
/// words, so a word count is a lower bound on tokens and capping it at 300
/// keeps the request conservatively inside the 500-token cap.
pub(crate) const KEYTERM_MAX_WORDS: usize = 300;

/// `&keyterm=` — the separator and key that precede every term on the wire.
const KEYTERM_PARAM_OVERHEAD: usize = 9;

/// Worst-case encoded byte length of one `keyterm` value.
///
/// Deliberately the worst case of the two encoders, because the two Deepgram
/// paths do not encode alike: the REST path hands params to `reqwest.query()`,
/// which **form**-encodes (space → `+`, `~` → `%7E`), while the streaming URI is
/// assembled by hand with [`percent_encode`] (RFC 3986: `~` stays raw, space →
/// `%20`). Measuring with either one alone under-counts the other — a
/// `~`-heavy vocabulary costed with `percent_encode` came out at half what the
/// REST request line actually carried, which put the request right back on the
/// ~8KB ceiling this budget exists to stay clear of. So: 3 bytes for any byte
/// *either* encoder escapes (`~` included, and space as `%20` rather than the
/// shorter `+`), 1 byte only for `A-Za-z0-9-_.`, which both leave alone.
fn keyterm_encoded_cost(value: &str) -> usize {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => 1,
            _ => 3,
        })
        .sum()
}

/// The vocabulary entries that are terms at all: trimmed, blanks dropped.
///
/// A hand-edited `config.toml` can hold blank entries and a bare `keyterm=` is
/// not a term. Kept separate from [`effective_keyterms`] only so callers can
/// report how many terms the *limits* dropped without counting blank config
/// entries as drops.
pub(crate) fn usable_keyterms(keyterms: &[String]) -> impl Iterator<Item = &str> {
    keyterms.iter().map(|t| t.trim()).filter(|t| !t.is_empty())
}

/// The `keyterm` values a Deepgram request actually carries, in order.
///
/// The single place the effective list is decided — `Config::validate` and
/// [`build_query_params`] both call this, so the count warned about at load is
/// by construction the count that goes on the wire. Normalization happens
/// first: entries are trimmed and blanks dropped *before* anything is charged,
/// then the remainder is charged against all three of
/// [`KEYTERM_QUERY_BUDGET_BYTES`], [`KEYTERM_MAX_TERMS`] and
/// [`KEYTERM_MAX_WORDS`]. Each bounds a different thing and none implies
/// another — see their own docs.
///
/// Ordering matters. An earlier split — count the budget over the raw list,
/// filter the blanks afterwards — let blank entries burn budget they never
/// spent: 200 blanks in front of a real vocabulary had `validate` advertise 335
/// terms while the request carried 135, with 105 real terms discarded for
/// nothing.
///
/// A term that does not fit is **skipped, not fatal**: iteration continues and
/// later, smaller terms can still make it. Stopping at the first misfit let one
/// pathological entry discard everything behind it —
/// `["x" × 5000, "whisrs", "Hyprland"]` put zero terms on the wire and warned
/// that "the first 0" reached Deepgram, while the same three terms reordered
/// kept both real ones. The consequence is that the result is no longer a
/// prefix of the input, so callers must report "N of M", never "the first N".
pub(crate) fn effective_keyterms(keyterms: &[String]) -> Vec<&str> {
    let mut used_bytes = 0usize;
    let mut used_words = 0usize;
    let mut effective = Vec::new();
    for term in usable_keyterms(keyterms) {
        if effective.len() >= KEYTERM_MAX_TERMS {
            break;
        }
        let cost = KEYTERM_PARAM_OVERHEAD + keyterm_encoded_cost(term);
        let words = term.split_whitespace().count();
        if used_bytes + cost > KEYTERM_QUERY_BUDGET_BYTES || used_words + words > KEYTERM_MAX_WORDS
        {
            continue;
        }
        used_bytes += cost;
        used_words += words;
        effective.push(term);
    }
    effective
}

/// Whether `model` accepts the `keyterm` parameter.
///
/// Deepgram answers `400 INVALID_QUERY_PARAMETER` — "`keyterm` is only
/// supported for Nova-3 and Flux" — on anything else, so a user who sets
/// `vocabulary` while pinning an older model must not have every request
/// rejected because of it. Pre-Nova-3 models bias through a different
/// parameter (`keywords`, with its own weighting syntax and its own
/// deprecation story); not wiring that up is deliberate scope for this
/// change, not an oversight.
pub(crate) fn supports_keyterm(model: &str) -> bool {
    model.starts_with("nova-3") || model.starts_with("flux")
}

/// What to log when the configured model cannot take the vocabulary at all, or
/// `None` when there is nothing to report.
///
/// A function rather than an inline condition so the emptiness test is
/// testable. This branch used to gate on `keyterms.is_empty()` while every
/// other count in this file goes through [`usable_keyterms`], so a
/// vocabulary of nothing but blank entries logged "ignoring 0 vocabulary
/// term(s)" on a nova-2 config.
fn unsupported_model_keyterm_notice(model: &str, keyterms: &[String]) -> Option<String> {
    let ignored = usable_keyterms(keyterms).count();
    if ignored == 0 {
        return None;
    }
    Some(format!(
        "model {model} does not support keyterm prompting — ignoring {ignored} vocabulary term(s)"
    ))
}

/// Build common query parameters for Deepgram requests.
fn build_query_params<'a>(
    config: &'a TranscriptionConfig,
    extra: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut params = vec![
        ("model", config.model.as_str()),
        ("language", map_language(&config.language)),
        ("smart_format", "true"),
    ];
    // Keyterm prompting: one repeated `keyterm` per term. Deepgram rejects
    // weights/intensifiers and comma- or semicolon-separated lists, and caps the
    // set at 500 tokens per request (see `KEYTERM_MAX_WORDS`).
    if supports_keyterm(&config.model) {
        let effective = effective_keyterms(&config.keyterms);
        let usable = usable_keyterms(&config.keyterms).count();
        if effective.len() < usable {
            // "N of M", never "the first N": `effective_keyterms` skips a term
            // that does not fit rather than stopping there, so the terms that
            // survive are not a prefix of the vocabulary.
            debug!(
                "vocabulary exceeds the Deepgram keyterm limits ({} bytes / {} terms / \
                 {} words) — sending {} of {} term(s)",
                KEYTERM_QUERY_BUDGET_BYTES,
                KEYTERM_MAX_TERMS,
                KEYTERM_MAX_WORDS,
                effective.len(),
                usable
            );
        }
        params.extend(effective.into_iter().map(|t| ("keyterm", t)));
    } else if let Some(notice) = unsupported_model_keyterm_notice(&config.model, &config.keyterms) {
        debug!("{notice}");
    }
    params.extend_from_slice(extra);
    params
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Top-level response from Deepgram's pre-recorded API.
#[derive(Debug, Deserialize)]
struct DeepgramResponse {
    results: DeepgramResults,
}

#[derive(Debug, Deserialize)]
struct DeepgramResults {
    channels: Vec<DeepgramChannel>,
}

#[derive(Debug, Deserialize)]
struct DeepgramChannel {
    alternatives: Vec<DeepgramAlternative>,
}

#[derive(Debug, Deserialize)]
struct DeepgramAlternative {
    transcript: String,
}

/// Error response from the Deepgram API.
#[derive(Debug, Deserialize)]
struct DeepgramErrorResponse {
    #[serde(default)]
    err_msg: String,
    #[serde(default)]
    err_code: String,
}

/// A streaming result message from the Deepgram WebSocket.
#[derive(Debug, Deserialize)]
struct StreamingResult {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    is_final: bool,
    #[serde(default)]
    channel: Option<StreamingChannel>,
}

#[derive(Debug, Deserialize)]
struct StreamingChannel {
    alternatives: Vec<DeepgramAlternative>,
}

// ===========================================================================
// REST backend (non-streaming)
// ===========================================================================

/// Deepgram REST transcription backend.
///
/// Sends the full WAV file to `/v1/listen` and returns the complete transcript.
pub struct DeepgramRestBackend {
    client: reqwest::Client,
    api_key: String,
}

impl DeepgramRestBackend {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
        }
    }
}

#[async_trait]
impl TranscriptionBackend for DeepgramRestBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        if audio.is_empty() {
            anyhow::bail!("cannot transcribe empty audio");
        }

        let api_key = resolve_api_key(&self.api_key)?;

        debug!(
            "sending {} bytes to Deepgram REST API (model={}, language={})",
            audio.len(),
            config.model,
            config.language
        );

        let params = build_query_params(config, &[]);

        let response = self
            .client
            .post(DEEPGRAM_REST_URL)
            .header("Authorization", format!("Token {api_key}"))
            .header("Content-Type", "audio/wav")
            .query(&params)
            .body(audio.to_vec())
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            if let Ok(err_resp) = serde_json::from_str::<DeepgramErrorResponse>(&body) {
                match status.as_u16() {
                    401 | 403 => {
                        anyhow::bail!("Deepgram API: invalid API key — {}", err_resp.err_msg)
                    }
                    429 => {
                        anyhow::bail!("Deepgram API: rate limited — {}", err_resp.err_msg)
                    }
                    _ => anyhow::bail!(
                        "Deepgram API error ({} {}): {}",
                        status.as_u16(),
                        err_resp.err_code,
                        err_resp.err_msg
                    ),
                }
            }
            anyhow::bail!("Deepgram API error ({}): {}", status.as_u16(), body);
        }

        let parsed: DeepgramResponse = serde_json::from_str(&body)?;
        let text = parsed
            .results
            .channels
            .first()
            .and_then(|ch| ch.alternatives.first())
            .map(|alt| alt.transcript.trim().to_string())
            .unwrap_or_default();

        if text.is_empty() {
            warn!("Deepgram returned empty transcription");
        }

        Ok(text)
    }

    // Uses the default transcribe_stream (collect + transcribe) since this
    // backend does not support streaming.

    // Deepgram has no prompt field; vocabulary rides as `keyterm` params.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        false
    }
}

// ===========================================================================
// Streaming backend (WebSocket)
// ===========================================================================

/// Deepgram streaming transcription backend.
///
/// Opens a WebSocket to Deepgram, sends raw PCM audio as binary frames,
/// and receives incremental transcription results. Only emits `is_final`
/// results to avoid duplicates.
pub struct DeepgramStreamingBackend {
    api_key: String,
}

impl DeepgramStreamingBackend {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

#[async_trait]
impl TranscriptionBackend for DeepgramStreamingBackend {
    async fn transcribe(
        &self,
        audio: &[u8],
        config: &TranscriptionConfig,
    ) -> anyhow::Result<String> {
        // For non-streaming use, set up the full WebSocket pipeline with a
        // single audio chunk, then collect the result.
        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(16);
        let (text_tx, mut text_rx) = mpsc::channel::<String>(16);

        // Decode WAV to get raw samples.
        let cursor = std::io::Cursor::new(audio);
        let reader = hound::WavReader::new(cursor)?;
        let samples: Vec<i16> = reader.into_samples::<i16>().collect::<Result<_, _>>()?;

        // Send all audio as one chunk, then close.
        audio_tx.send(samples).await.ok();
        drop(audio_tx);

        let config_clone = config.clone();
        let stream_result = self.transcribe_stream(audio_rx, text_tx, &config_clone);

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

    async fn transcribe_stream(
        &self,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
        text_tx: mpsc::Sender<String>,
        config: &TranscriptionConfig,
    ) -> anyhow::Result<()> {
        let api_key = resolve_api_key(&self.api_key)?;

        let params = build_query_params(
            config,
            &[
                ("encoding", "linear16"),
                ("sample_rate", "16000"),
                ("channels", "1"),
                ("interim_results", "false"),
            ],
        );

        // Build the WebSocket URL with query parameters.
        let query_string = build_query_string(&params);
        let url = format!("{DEEPGRAM_WS_URL}?{query_string}");

        info!("connecting to Deepgram streaming API");

        let request = tungstenite::http::Request::builder()
            .uri(&url)
            .header("Authorization", format!("Token {api_key}"))
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .header("Sec-WebSocket-Version", "13")
            .header("Host", "api.deepgram.com")
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .body(())?;

        let (ws_stream, _response) = tokio_tungstenite::connect_async(request).await?;
        let (mut ws_sink, mut ws_source) = ws_stream.split();

        info!("connected to Deepgram streaming API");

        // Spawn a task to send audio as raw PCM binary frames.
        let send_task = tokio::spawn(async move {
            while let Some(chunk) = audio_rx.recv().await {
                // Convert i16 samples to little-endian bytes.
                let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();

                if ws_sink
                    .send(tungstenite::Message::Binary(bytes.into()))
                    .await
                    .is_err()
                {
                    error!("Deepgram WebSocket send failed — connection may be closed");
                    break;
                }
            }

            // Signal end of audio stream.
            debug!("sending CloseStream to Deepgram");
            let close_msg = r#"{"type":"CloseStream"}"#;
            ws_sink
                .send(tungstenite::Message::Text(close_msg.into()))
                .await
                .ok();
        });

        // Receive transcription results.
        let timeout_duration = std::time::Duration::from_secs(15);
        while let Ok(Some(msg_result)) =
            tokio::time::timeout(timeout_duration, ws_source.next()).await
        {
            match msg_result {
                Ok(tungstenite::Message::Text(text)) => {
                    match serde_json::from_str::<StreamingResult>(&text) {
                        Ok(result) => match result.msg_type.as_str() {
                            "Results" => {
                                // Only emit final results to avoid duplicates.
                                if result.is_final {
                                    let transcript = result
                                        .channel
                                        .and_then(|ch| ch.alternatives.into_iter().next())
                                        .map(|alt| alt.transcript.trim().to_string())
                                        .unwrap_or_default();

                                    if !transcript.is_empty() {
                                        debug!("deepgram final: {transcript}");
                                        text_tx.send(transcript).await.ok();
                                    }
                                }
                            }
                            "Metadata" => {
                                debug!("deepgram metadata received");
                            }
                            "SpeechStarted" => {
                                debug!("deepgram speech started");
                            }
                            "UtteranceEnd" => {
                                debug!("deepgram utterance end");
                            }
                            other => {
                                debug!("unhandled Deepgram message type: {other}");
                            }
                        },
                        Err(e) => {
                            debug!("failed to parse Deepgram message: {e}");
                            debug!("raw message: {text}");
                        }
                    }
                }
                Ok(tungstenite::Message::Close(_)) => {
                    info!("Deepgram WebSocket closed by server");
                    break;
                }
                Err(e) => {
                    error!("Deepgram WebSocket receive error: {e}");
                    break;
                }
                _ => {}
            }
        }

        send_task.await.ok();
        info!("Deepgram stream finished");

        Ok(())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    // Same as the REST backend: no prompt field on the wire, vocabulary rides
    // as `keyterm` query params.
    fn sends_prompt(&self, _config: &TranscriptionConfig) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepgram_backends_never_send_the_prompt() {
        let request = TranscriptionConfig {
            language: "en".to_string(),
            model: "nova-3".to_string(),
            prompt: Some("Hyprland, whisrs".to_string()),
            keyterms: Vec::new(),
        };

        assert!(!DeepgramRestBackend::new(String::new()).sends_prompt(&request));
        assert!(!DeepgramStreamingBackend::new(String::new()).sends_prompt(&request));
    }

    #[test]
    fn map_language_auto_to_multi() {
        assert_eq!(map_language("auto"), "multi");
    }

    #[test]
    fn map_language_passthrough() {
        assert_eq!(map_language("en"), "en");
        assert_eq!(map_language("fr"), "fr");
        assert_eq!(map_language("ja"), "ja");
    }

    #[test]
    fn parse_rest_response() {
        let body = r#"{
            "metadata": {"request_id": "test"},
            "results": {
                "channels": [{
                    "alternatives": [{
                        "transcript": "Hello world.",
                        "confidence": 0.98
                    }]
                }]
            }
        }"#;
        let parsed: DeepgramResponse = serde_json::from_str(body).unwrap();
        assert_eq!(
            parsed.results.channels[0].alternatives[0].transcript,
            "Hello world."
        );
    }

    #[test]
    fn parse_rest_response_empty_transcript() {
        let body = r#"{
            "results": {
                "channels": [{
                    "alternatives": [{
                        "transcript": "",
                        "confidence": 0.0
                    }]
                }]
            }
        }"#;
        let parsed: DeepgramResponse = serde_json::from_str(body).unwrap();
        assert!(parsed.results.channels[0].alternatives[0]
            .transcript
            .is_empty());
    }

    #[test]
    fn parse_error_response() {
        let body = r#"{"err_msg": "Invalid credentials", "err_code": "INVALID_AUTH"}"#;
        let parsed: DeepgramErrorResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.err_msg, "Invalid credentials");
        assert_eq!(parsed.err_code, "INVALID_AUTH");
    }

    #[test]
    fn parse_streaming_result_final() {
        let body = r#"{
            "type": "Results",
            "channel_index": [0, 1],
            "duration": 1.5,
            "start": 0.0,
            "is_final": true,
            "speech_final": true,
            "channel": {
                "alternatives": [{
                    "transcript": "Hello world.",
                    "confidence": 0.98
                }]
            }
        }"#;
        let parsed: StreamingResult = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.msg_type, "Results");
        assert!(parsed.is_final);
        let transcript = &parsed.channel.unwrap().alternatives[0].transcript;
        assert_eq!(transcript, "Hello world.");
    }

    #[test]
    fn parse_streaming_result_interim() {
        let body = r#"{
            "type": "Results",
            "is_final": false,
            "channel": {
                "alternatives": [{"transcript": "Hel", "confidence": 0.5}]
            }
        }"#;
        let parsed: StreamingResult = serde_json::from_str(body).unwrap();
        assert!(!parsed.is_final);
    }

    #[test]
    fn parse_streaming_metadata() {
        let body = r#"{"type": "Metadata", "request_id": "abc123"}"#;
        let parsed: StreamingResult = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.msg_type, "Metadata");
    }

    /// A [`TranscriptionConfig`] with just the fields the query builder reads.
    fn query_config(model: &str, keyterms: &[&str]) -> TranscriptionConfig {
        TranscriptionConfig {
            language: "en".to_string(),
            model: model.to_string(),
            prompt: None,
            keyterms: keyterms.iter().map(|t| t.to_string()).collect(),
        }
    }

    /// Same as [`query_config`] for a vocabulary that is already owned.
    fn owned_query_config(model: &str, keyterms: &[String]) -> TranscriptionConfig {
        TranscriptionConfig {
            language: "en".to_string(),
            model: model.to_string(),
            prompt: None,
            keyterms: keyterms.to_vec(),
        }
    }

    /// The `keyterm` values `build_query_params` put on the wire, in order.
    fn keyterm_values<'a>(params: &'a [(&'a str, &'a str)]) -> Vec<&'a str> {
        params
            .iter()
            .filter(|(k, _)| *k == "keyterm")
            .map(|(_, v)| *v)
            .collect()
    }

    #[tokio::test]
    async fn rest_rejects_empty_audio() {
        let backend = DeepgramRestBackend::new("test-key".to_string());
        let config = query_config("nova-3", &[]);
        let err = backend.transcribe(&[], &config).await.unwrap_err();
        assert!(err.to_string().contains("empty audio"));
    }

    #[test]
    fn build_query_params_includes_smart_format() {
        let config = query_config("nova-3", &[]);
        let params = build_query_params(&config, &[]);
        assert!(params
            .iter()
            .any(|(k, v)| *k == "smart_format" && *v == "true"));
        assert!(params.iter().any(|(k, v)| *k == "model" && *v == "nova-3"));
        assert!(params.iter().any(|(k, v)| *k == "language" && *v == "en"));
    }

    #[test]
    fn build_query_params_auto_language() {
        let mut config = query_config("nova-3", &[]);
        config.language = "auto".to_string();
        let params = build_query_params(&config, &[]);
        assert!(params
            .iter()
            .any(|(k, v)| *k == "language" && *v == "multi"));
    }

    #[test]
    fn build_query_params_emits_one_keyterm_per_vocabulary_entry() {
        let config = query_config("nova-3", &["whisrs", "GNOME Shell"]);
        let params = build_query_params(&config, &[]);
        assert_eq!(keyterm_values(&params), vec!["whisrs", "GNOME Shell"]);
    }

    #[test]
    fn build_query_params_omits_keyterm_on_models_that_reject_it() {
        // nova-2 answers 400 INVALID_QUERY_PARAMETER when `keyterm` is present,
        // so a vocabulary set alongside an older model must be dropped rather
        // than break every request.
        let config = query_config("nova-2", &["whisrs"]);
        let params = build_query_params(&config, &[]);
        assert!(!params.iter().any(|(k, _)| *k == "keyterm"));
    }

    #[test]
    fn build_query_params_without_vocabulary_sends_no_keyterm() {
        let config = query_config("nova-3", &[]);
        let params = build_query_params(&config, &[]);
        assert!(!params.iter().any(|(k, _)| *k == "keyterm"));
    }

    #[test]
    fn build_query_params_skips_blank_vocabulary_entries() {
        // `whisrs config` strips these, but a hand-edited config.toml can hold
        // them and a bare `keyterm=` is not a term.
        let config = query_config("nova-3", &["whisrs", "", "  ", "\t"]);
        let params = build_query_params(&config, &[]);
        assert_eq!(keyterm_values(&params), vec!["whisrs"]);
    }

    #[test]
    fn build_query_params_sends_keyterms_trimmed() {
        let config = query_config("nova-3", &["  GNOME Shell  "]);
        let params = build_query_params(&config, &[]);
        assert_eq!(keyterm_values(&params), vec!["GNOME Shell"]);
    }

    #[test]
    fn build_query_params_truncates_vocabulary_to_the_query_budget() {
        // 1000 terms builds a ~28KB URI; Deepgram's edge rejects the handshake
        // with a bare 400 that never mentions the vocabulary. Terms are short
        // enough here that the byte budget bites before `KEYTERM_MAX_TERMS`
        // would — 4096 / (9 + 8) is 240, above the 200-term cap, so keep the
        // assertion on the cap that actually fires.
        let terms: Vec<String> = (0..1000).map(|i| format!("term{i:04}")).collect();
        let config = owned_query_config("nova-3", &terms);
        let params = build_query_params(&config, &[]);
        let sent = keyterm_values(&params);
        assert!(
            sent.len() < terms.len(),
            "budget must drop terms, sent all {}",
            sent.len()
        );
        assert!(!sent.is_empty(), "budget must still send the leading terms");
        // The terms it does send are a prefix, and they fit the budget.
        assert_eq!(sent[0], "term0000");
        let bytes: usize = sent
            .iter()
            .map(|t| KEYTERM_PARAM_OVERHEAD + keyterm_encoded_cost(t))
            .sum();
        assert!(
            bytes <= KEYTERM_QUERY_BUDGET_BYTES,
            "sent {bytes} bytes of keyterms, over the {KEYTERM_QUERY_BUDGET_BYTES} budget"
        );
    }

    #[test]
    fn effective_keyterms_admits_a_small_vocabulary_whole() {
        let terms = vec!["whisrs".to_string(), "GNOME Shell".to_string()];
        assert_eq!(effective_keyterms(&terms), vec!["whisrs", "GNOME Shell"]);
    }

    #[test]
    fn effective_keyterms_is_what_build_query_params_puts_on_the_wire() {
        // The bug this pins: blanks used to be charged against the budget and
        // filtered afterwards, so the advertised count and the wire disagreed.
        // 200 whitespace-only entries in front of 1000 real terms had
        // `validate` promise 335 while the request carried 135 — 105 real terms
        // discarded and 1801 budget bytes burned on nothing.
        let mut terms: Vec<String> = vec!["   ".to_string(); 200];
        terms.extend((0..1000).map(|i| format!("term{i:04}")));

        let config = owned_query_config("nova-3", &terms);
        let params = build_query_params(&config, &[]);
        let sent = keyterm_values(&params);
        let advertised = effective_keyterms(&terms);

        assert_eq!(
            sent, advertised,
            "the advertised list must be the list on the wire"
        );
        assert!(
            sent.iter().all(|t| !t.trim().is_empty()),
            "a blank entry reached the wire: {sent:?}"
        );
        assert_eq!(
            sent[0], "term0000",
            "blanks must not shift which real terms are kept"
        );
        // Blanks cost nothing, so the real terms get the whole budget: the same
        // 1000 terms with no blanks in front produce exactly the same list.
        let without_blanks: Vec<String> = (0..1000).map(|i| format!("term{i:04}")).collect();
        assert_eq!(
            advertised,
            effective_keyterms(&without_blanks),
            "blank entries still consumed budget"
        );

        // And the number `Config::validate` advertises at load is the number of
        // `keyterm` params this same vocabulary puts on the wire. This is the
        // whole point of routing both through `effective_keyterms`: the two
        // used to be computed separately and disagreed.
        let mut config: crate::Config = toml::from_str("").expect("empty config uses defaults");
        config.general.backend = "deepgram".to_string();
        config.general.vocabulary = terms.clone();
        config.deepgram = Some(crate::DeepgramConfig {
            api_key: "test-key".to_string(),
            model: "nova-3".to_string(),
        });
        let warnings = config.validate().expect("a keyed deepgram config is valid");
        let warning = warnings
            .iter()
            .find(|w| w.message.contains("vocabulary"))
            .unwrap_or_else(|| panic!("a truncated vocabulary must warn: {warnings:?}"));
        assert!(
            warning.message.contains(&format!("{} of ", sent.len())),
            "validate advertised a different count than the {} terms on the wire: {}",
            sent.len(),
            warning.message
        );
    }

    #[test]
    fn effective_keyterms_caps_the_word_count_before_the_byte_budget() {
        // The claim `KEYTERM_MAX_TERMS` used to make — that staying under 200
        // terms keeps a byte-legal vocabulary inside Deepgram's 500-token cap —
        // is false, and this is the counterexample. 400 copies of "ab cd ef"
        // stop at 195 terms on the byte budget alone: under the term cap, at
        // 4095 of 4096 bytes, and carrying at least 585 whitespace-delimited
        // words. `KEYTERM_MAX_WORDS` is what actually bounds it.
        let terms: Vec<String> = vec!["ab cd ef".to_string(); 400];

        // What the other two limits alone would have allowed.
        let cost = KEYTERM_PARAM_OVERHEAD + keyterm_encoded_cost("ab cd ef");
        let byte_bound = KEYTERM_QUERY_BUDGET_BYTES / cost;
        assert!(
            byte_bound < KEYTERM_MAX_TERMS,
            "fixture must be byte-bound below the term cap, got {byte_bound}"
        );
        assert!(
            byte_bound * 3 > 500,
            "fixture must blow the 500-token cap without the word limit: {} words",
            byte_bound * 3
        );

        let effective = effective_keyterms(&terms);
        assert_eq!(
            effective.len(),
            KEYTERM_MAX_WORDS / 3,
            "the word limit, not the byte budget, must be what bites"
        );
        let words: usize = effective.iter().map(|t| t.split_whitespace().count()).sum();
        assert!(
            words <= KEYTERM_MAX_WORDS,
            "sent {words} words, over the {KEYTERM_MAX_WORDS} cap"
        );

        // And the count that goes on the wire is the count `validate` advertises.
        let config = owned_query_config("nova-3", &terms);
        let params = build_query_params(&config, &[]);
        let sent = keyterm_values(&params);
        assert_eq!(sent.len(), effective.len());

        let mut config: crate::Config = toml::from_str("").expect("empty config uses defaults");
        config.general.backend = "deepgram".to_string();
        config.general.vocabulary = terms.clone();
        config.deepgram = Some(crate::DeepgramConfig {
            api_key: "test-key".to_string(),
            model: "nova-3".to_string(),
        });
        let warnings = config.validate().expect("a keyed deepgram config is valid");
        let warning = warnings
            .iter()
            .find(|w| w.message.contains("vocabulary"))
            .unwrap_or_else(|| panic!("a word-capped vocabulary must warn: {warnings:?}"));
        assert!(
            warning
                .message
                .contains(&format!("{} of {} usable term(s)", sent.len(), terms.len())),
            "validate advertised a different count than the {} terms on the wire: {}",
            sent.len(),
            warning.message
        );
    }

    #[test]
    fn one_oversized_term_does_not_discard_the_rest_of_the_vocabulary() {
        // `effective_keyterms` used to `break` on the first term that would not
        // fit, so a single pathological entry nuked everything behind it: this
        // vocabulary put zero terms on the wire and warned that "the first 0"
        // reached Deepgram, while the same three terms reordered kept both real
        // ones. Skipping the misfit and continuing is what makes the outcome
        // independent of where the long term sits.
        let long = "x".repeat(5000);
        let first = vec![long.clone(), "whisrs".to_string(), "Hyprland".to_string()];
        let last = vec!["whisrs".to_string(), "Hyprland".to_string(), long];

        assert!(
            KEYTERM_PARAM_OVERHEAD + keyterm_encoded_cost(&"x".repeat(5000))
                > KEYTERM_QUERY_BUDGET_BYTES,
            "the fixture term must not fit on its own"
        );
        assert_eq!(effective_keyterms(&first), vec!["whisrs", "Hyprland"]);
        assert_eq!(effective_keyterms(&last), vec!["whisrs", "Hyprland"]);

        let config = owned_query_config("nova-3", &first);
        let params = build_query_params(&config, &[]);
        assert_eq!(keyterm_values(&params), vec!["whisrs", "Hyprland"]);
    }

    #[test]
    fn unsupported_model_notice_uses_the_same_emptiness_test_as_the_rest_of_the_file() {
        // The unsupported-model branch used to gate on `keyterms.is_empty()`
        // while every other count here goes through `usable_keyterms`, so a
        // blanks-only vocabulary logged "ignoring 0 vocabulary term(s)".
        let blanks = ["".to_string(), "  ".to_string(), "\t".to_string()];
        assert!(!blanks.is_empty(), "the old gate would have fired here");
        assert_eq!(unsupported_model_keyterm_notice("nova-2", &blanks), None);
        assert_eq!(unsupported_model_keyterm_notice("nova-2", &[]), None);

        let notice =
            unsupported_model_keyterm_notice("nova-2", &["whisrs".to_string(), "   ".to_string()])
                .expect("a real term must be reported");
        assert!(
            notice.contains("ignoring 1 vocabulary term(s)"),
            "blank entries must not be counted: {notice}"
        );
        assert!(notice.contains("nova-2"), "the notice must name the model");

        // And a blanks-only vocabulary still puts no `keyterm` on the wire.
        let config = query_config("nova-2", &["", "  ", "\t"]);
        let params = build_query_params(&config, &[]);
        assert!(!params.iter().any(|(k, _)| *k == "keyterm"));
    }

    #[test]
    fn keyterm_cost_charges_three_bytes_for_anything_either_encoder_escapes() {
        // `~` is the trap: RFC 3986 leaves it raw (so `percent_encode` charges
        // 1) but `reqwest.query()` form-encodes it to `%7E`. Costing with
        // `percent_encode` let a `~`-heavy vocabulary put ~2x the counted bytes
        // on the REST request line, right back on the ~8KB ceiling.
        assert_eq!(keyterm_encoded_cost("~~~~~~~~"), 24);
        assert_eq!(percent_encode("~~~~~~~~").len(), 8, "RFC 3986 leaves ~ raw");
        // Space is charged as `%20`, not the shorter form-encoded `+`.
        assert_eq!(keyterm_encoded_cost("GNOME Shell"), 13);
        // The unreserved set both encoders agree on stays at one byte each.
        assert_eq!(keyterm_encoded_cost("nova-3_v.1"), 10);
    }

    #[test]
    fn keyterm_budget_is_not_overrun_by_a_tilde_heavy_vocabulary() {
        // 300 copies of `~~~~~~~~`: the whole point is that what the budget
        // counts is an upper bound on what the form-encoding REST path emits.
        let terms: Vec<String> = vec!["~~~~~~~~".to_string(); 300];
        let effective = effective_keyterms(&terms);
        let counted: usize = effective
            .iter()
            .map(|t| KEYTERM_PARAM_OVERHEAD + keyterm_encoded_cost(t))
            .sum();
        // What reqwest's form encoding actually puts on the REST request line.
        let form_encoded: usize = effective
            .iter()
            .map(|t| KEYTERM_PARAM_OVERHEAD + t.len() * "%7E".len())
            .sum();
        assert!(
            form_encoded <= counted,
            "REST carries {form_encoded} bytes but the budget only counted {counted}"
        );
        assert!(
            counted <= KEYTERM_QUERY_BUDGET_BYTES,
            "counted {counted} bytes, over the {KEYTERM_QUERY_BUDGET_BYTES} budget"
        );
    }

    #[test]
    fn effective_keyterms_caps_the_term_count_well_inside_the_byte_budget() {
        // Deepgram's real limit is 500 tokens, which the byte budget does not
        // bound. These terms are short enough that the first 200 of them use
        // barely half the byte budget, so if `KEYTERM_MAX_TERMS` were not
        // enforced the byte budget would let ~315 through instead of 200.
        let terms: Vec<String> = (0..500).map(|i| format!("t{i:03}")).collect();
        let bytes: usize = terms
            .iter()
            .take(KEYTERM_MAX_TERMS)
            .map(|t| KEYTERM_PARAM_OVERHEAD + keyterm_encoded_cost(t))
            .sum();
        assert!(
            bytes < KEYTERM_QUERY_BUDGET_BYTES,
            "fixture must be inside the byte budget so the term cap is what fires"
        );
        assert_eq!(effective_keyterms(&terms).len(), KEYTERM_MAX_TERMS);

        let config = owned_query_config("nova-3", &terms);
        let params = build_query_params(&config, &[]);
        let sent = keyterm_values(&params);
        assert_eq!(sent.len(), KEYTERM_MAX_TERMS);
    }

    #[test]
    fn percent_encode_escapes_spaces_and_leaves_plain_values_alone() {
        // A raw space in a multi-word keyterm makes the streaming URI invalid
        // and Deepgram answers the handshake with a bare 400.
        assert_eq!(percent_encode("GNOME Shell"), "GNOME%20Shell");
        assert_eq!(percent_encode("nova-3"), "nova-3");
        assert_eq!(percent_encode("linear16"), "linear16");
    }

    #[test]
    fn build_query_string_percent_encodes_values() {
        // The streaming URI is assembled from these params by hand, so the
        // encoding has to survive the join, not just `percent_encode`.
        let config = query_config("nova-3", &["GNOME Shell"]);
        let params = build_query_params(&config, &[]);
        let query = build_query_string(&params);
        assert!(
            query.contains("keyterm=GNOME%20Shell"),
            "multi-word keyterm must be percent-encoded: {query}"
        );
        assert!(
            !query.contains("GNOME Shell"),
            "raw space left in the query string: {query}"
        );
        assert!(
            query.contains("model=nova-3"),
            "the fixed params must survive encoding: {query}"
        );
    }
}
