//! Waiting out rate limits (HTTP 429/503) and riding out server errors
//! (500, 502, 504, Cloudflare's 520 to 524) and network failures for long
//! jobs such as recordings. Only recording requests go through here;
//! dictation does not retry.
//!
//! One retry layer per request: [`retry_rate_limited`] retries an operation
//! that fails with a [`RetryableHttpError`] or a transport error
//! ([`is_transport_error`]), and when its retries run out it returns a
//! [`RateLimitExhausted`] or a [`NetworkUnavailable`], neither of which is
//! retryable. So a caller that wraps an operation already retried inside (a
//! phrase backend retries each phrase in [`super::segments_by_phrase`])
//! gives up at once instead of multiplying the wait.

use std::future::Future;
use std::time::Duration;

use tracing::info;

use super::RetryableHttpError;

/// Longest one request waits out rate limits before giving up.
pub const RATE_LIMIT_BUDGET: Duration = Duration::from_secs(300);

/// First backoff without a `Retry-After`, doubled per attempt up to
/// [`BACKOFF_MAX`].
pub const BACKOFF_BASE: Duration = Duration::from_secs(2);
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// The wait before each retry of a server error (500, 502, 504, 520 to 524)
/// that names no `Retry-After`. Only these few: a server that keeps failing
/// is not a rate limit about to lift, and all three stay well inside
/// [`RATE_LIMIT_BUDGET`].
pub const SERVER_ERROR_BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
];

/// The wait before each retry of a request that got no HTTP answer at all
/// ([`is_transport_error`]): longer than [`SERVER_ERROR_BACKOFF`], since a
/// dropped Wi-Fi or a VPN that reconnects takes a while to come back. All
/// four stay inside [`RATE_LIMIT_BUDGET`].
pub const NETWORK_ERROR_BACKOFF: [Duration; 4] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

/// A request that was still failing (rate limited, overloaded, or a server
/// error) when its retries ran out. Not a [`RetryableHttpError`], so an
/// outer retry layer does not retry it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitExhausted {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for RateLimitExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RateLimitExhausted {}

/// A request that still could not reach the service when its network
/// retries ([`NETWORK_ERROR_BACKOFF`]) ran out. Like [`RateLimitExhausted`]
/// it is final for an outer retry layer; a recording that ends on it waits
/// for the network instead of failing ([`is_network_unavailable`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkUnavailable {
    pub message: String,
}

impl std::fmt::Display for NetworkUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NetworkUnavailable {}

/// Whether `err` is a transport failure, a request that got no HTTP answer:
/// the name did not resolve, the connection was refused, reset or timed
/// out, the TLS handshake failed, or the connection broke while the answer
/// was read. The one place these are told apart from an answer: any HTTP
/// status, a 401 or a 400 included, is not one of them (see
/// [`RetryableHttpError`] for the statuses worth retrying), and neither is
/// a body that arrived whole but did not parse.
pub fn is_transport_error(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(is_transport)
}

fn is_transport(e: &reqwest::Error) -> bool {
    e.is_connect()
        || e.is_timeout()
        || e.is_request()
        || e.is_body()
        || (e.is_decode() && broke_off(e))
}

/// The error of a request to a transcription server the user runs (the
/// `asr-sidecar`: on this machine, or a gateway such as LiteLLM or Speaches
/// elsewhere). When the server took the request and then closed the
/// connection ([`closed_by_peer`]), it died handling it (out of memory, a
/// crash in the model), and the same audio would take it down again: that
/// is final, like the sidecar's HTTP 500, and no longer carries the reqwest
/// error a retry layer would act on. Everything else stays as it was: a
/// refused connection (not running yet), a timeout or an unreachable host
/// (the network) are waited out like any transport error. The cloud
/// backends post to fixed hosts and keep [`is_transport_error`] as it is.
pub fn final_if_crashed(err: reqwest::Error, server: &str) -> anyhow::Error {
    if err.is_connect() || !is_transport(&err) || !closed_by_peer(&err) {
        return err.into();
    }
    anyhow::anyhow!(
        "{server} closed the connection while handling the request, so it likely crashed \
         on this audio; not sent again: {:#}",
        anyhow::Error::from(err)
    )
}

/// What hyper says when the server closed the connection before its answer
/// was complete. That error carries no I/O error to read the kind from;
/// `a_sidecar_that_is_down_is_waited_out_but_a_crash_is_final` pins the text.
const INCOMPLETE_MESSAGE: &str = "connection closed before message completed";

/// Whether `err` or a cause under it is the other end closing the
/// connection: reset, broken pipe, an end of file in the middle of the
/// answer, or hyper's [`INCOMPLETE_MESSAGE`]. A timeout, an unreachable host
/// or network, and a refused connection are not.
fn closed_by_peer(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(err);
    while let Some(e) = cause {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ) {
                return true;
            }
        }
        if e.to_string() == INCOMPLETE_MESSAGE {
            return true;
        }
        cause = e.source();
    }
    false
}

/// Whether a reqwest decode error is the answer breaking off, an I/O error
/// under it (reqwest reports a failed body read as a decode error), rather
/// than a body that arrived whole and did not parse.
fn broke_off(err: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        if cause.is::<std::io::Error>() {
            return true;
        }
        source = cause.source();
    }
    false
}

/// Whether `err` carries a [`NetworkUnavailable`]: a request gave up on the
/// network.
pub fn is_network_unavailable(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| cause.is::<NetworkUnavailable>())
}

/// How long to wait before retrying a rate-limited (429) or overloaded
/// (503) request, or `None` to give up. Honors the server's `Retry-After`;
/// otherwise backs off exponentially from [`BACKOFF_BASE`] to
/// [`BACKOFF_MAX`]. Gives up once the wait would push the total past
/// `budget`.
pub fn backoff_delay(
    attempt: u32,
    retry_after: Option<Duration>,
    waited: Duration,
    budget: Duration,
) -> Option<Duration> {
    let exponential = BACKOFF_BASE
        .saturating_mul(1u32 << attempt.min(16))
        .min(BACKOFF_MAX);
    // A zero `Retry-After` still gets a short pause, so a confused server
    // cannot make this spin.
    let delay = retry_after.map_or(exponential, |d| d.max(Duration::from_millis(500)));
    (waited.saturating_add(delay) <= budget).then_some(delay)
}

/// How long to wait before retry number `retries` (from 0) of a server
/// error, or `None` to give up: the `schedule` entry, or the server's
/// `Retry-After` when it sent one, while the schedule lasts and the total
/// stays within `budget`. A network error goes by its own schedule
/// ([`NETWORK_ERROR_BACKOFF`]) the same way, with no `Retry-After`.
pub fn server_error_delay(
    schedule: &[Duration],
    retries: usize,
    retry_after: Option<Duration>,
    waited: Duration,
    budget: Duration,
) -> Option<Duration> {
    let planned = *schedule.get(retries)?;
    let delay = retry_after.map_or(planned, |d| d.max(Duration::from_millis(500)));
    (waited.saturating_add(delay) <= budget).then_some(delay)
}

/// Why a retry gave up, in words.
fn give_up_message(retryable: &RetryableHttpError, waited: Duration, budget: Duration) -> String {
    let status = retryable.status;
    let kind = if retryable.is_rate_limit() {
        "rate limited"
    } else {
        "server error"
    };
    match retryable.retry_after {
        Some(asked) if waited.saturating_add(asked) > budget => {
            let mut message = format!(
                "{kind} (HTTP {status}); the server asked to wait {} s, more than the \
                 {} s budget",
                asked.as_secs_f64().ceil() as u64,
                budget.as_secs()
            );
            if !waited.is_zero() {
                message.push_str(&format!(" after {} s of retries", waited.as_secs()));
            }
            format!("{message}: {}", retryable.message)
        }
        _ => format!(
            "the backend still answered HTTP {status} after {} s of retries: {}",
            waited.as_secs(),
            retryable.message
        ),
    }
}

/// Run `op`, waiting out rate limits within `budget`, retrying a server
/// error up to [`SERVER_ERROR_BACKOFF`] times and a transport error up to
/// [`NETWORK_ERROR_BACKOFF`] times.
///
/// Any other error is returned as is. When the retries run out the result
/// is a [`RateLimitExhausted`] or a [`NetworkUnavailable`] ending in the last
/// error's message.
pub async fn retry_rate_limited<T, F, Fut>(budget: Duration, op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    retry_transient(budget, &SERVER_ERROR_BACKOFF, op).await
}

/// [`retry_rate_limited`] with the server-error waits given, so a test
/// against a real socket need not sleep through them.
pub(crate) async fn retry_transient<T, F, Fut>(
    budget: Duration,
    server_backoff: &[Duration],
    op: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    retry_with_backoff(budget, server_backoff, &NETWORK_ERROR_BACKOFF, op).await
}

/// What a failed attempt was, for picking its wait.
enum Failure {
    /// 429 or 503.
    RateLimited(RetryableHttpError),
    /// Another retryable status.
    Server(RetryableHttpError),
    /// No HTTP answer at all ([`is_transport_error`]).
    Network,
}

/// [`retry_transient`] with the network-error waits given too.
pub(crate) async fn retry_with_backoff<T, F, Fut>(
    budget: Duration,
    server_backoff: &[Duration],
    network_backoff: &[Duration],
    mut op: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut attempt = 0u32;
    let mut server_retries = 0usize;
    let mut network_retries = 0usize;
    let mut waited = Duration::ZERO;
    loop {
        let err = match op().await {
            Ok(value) => return Ok(value),
            Err(e) => e,
        };
        let failure = match err
            .chain()
            .find_map(|c| c.downcast_ref::<RetryableHttpError>())
            .cloned()
        {
            Some(retryable) if retryable.is_rate_limit() => Failure::RateLimited(retryable),
            Some(retryable) => Failure::Server(retryable),
            None if is_transport_error(&err) => Failure::Network,
            None => return Err(err),
        };
        let delay = match &failure {
            Failure::RateLimited(r) => backoff_delay(attempt, r.retry_after, waited, budget),
            Failure::Server(r) => server_error_delay(
                server_backoff,
                server_retries,
                r.retry_after,
                waited,
                budget,
            ),
            Failure::Network => {
                server_error_delay(network_backoff, network_retries, None, waited, budget)
            }
        };
        let Some(delay) = delay else {
            return Err(match failure {
                Failure::RateLimited(r) | Failure::Server(r) => RateLimitExhausted {
                    status: r.status,
                    message: give_up_message(&r, waited, budget),
                }
                .into(),
                Failure::Network => NetworkUnavailable {
                    message: format!(
                        "could not reach the transcription service after {} s of retries: {}",
                        waited.as_secs(),
                        err.root_cause()
                    ),
                }
                .into(),
            });
        };
        match &failure {
            Failure::RateLimited(r) | Failure::Server(r) => info!(
                "transcription got HTTP {}; retrying in {:.1} s",
                r.status,
                delay.as_secs_f64()
            ),
            Failure::Network => info!(
                "transcription could not reach the service ({}); retrying in {:.1} s",
                err.root_cause(),
                delay.as_secs_f64()
            ),
        }
        tokio::time::sleep(delay).await;
        waited += delay;
        match failure {
            Failure::RateLimited(_) => attempt += 1,
            Failure::Server(_) => server_retries += 1,
            Failure::Network => network_retries += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn limited(retry_after: Option<u64>) -> anyhow::Error {
        RetryableHttpError {
            status: 429,
            retry_after: retry_after.map(Duration::from_secs),
            message: "slow down".into(),
        }
        .into()
    }

    #[test]
    fn backoff_honors_retry_after_and_the_budget() {
        let budget = Duration::from_secs(300);
        // Exponential without a Retry-After, capped.
        assert_eq!(
            backoff_delay(0, None, Duration::ZERO, budget),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            backoff_delay(3, None, Duration::ZERO, budget),
            Some(Duration::from_secs(16))
        );
        assert_eq!(
            backoff_delay(30, None, Duration::ZERO, budget),
            Some(BACKOFF_MAX)
        );
        // The server's wait wins, even when longer than the backoff.
        assert_eq!(
            backoff_delay(0, Some(Duration::from_secs(90)), Duration::ZERO, budget),
            Some(Duration::from_secs(90))
        );
        // A zero Retry-After still pauses.
        assert_eq!(
            backoff_delay(0, Some(Duration::ZERO), Duration::ZERO, budget),
            Some(Duration::from_millis(500))
        );
        // Past the budget: give up.
        assert_eq!(
            backoff_delay(
                0,
                Some(Duration::from_secs(61)),
                Duration::from_secs(240),
                budget
            ),
            None
        );
        assert_eq!(
            backoff_delay(0, None, Duration::from_secs(299), budget),
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_after_beyond_the_budget_says_so() {
        let calls = AtomicUsize::new(0);
        let err = retry_rate_limited(RATE_LIMIT_BUDGET, || {
            calls.fetch_add(1, Ordering::Relaxed);
            async { Err::<(), _>(limited(Some(900))) }
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            err.to_string(),
            "rate limited (HTTP 429); the server asked to wait 900 s, more than the 300 s \
             budget: slow down"
        );
        // Exhausted is final: an outer layer must not retry it.
        assert!(err.downcast_ref::<RetryableHttpError>().is_none());
        assert!(err.downcast_ref::<RateLimitExhausted>().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn endless_backoff_reports_the_time_waited() {
        let started = tokio::time::Instant::now();
        let err = retry_rate_limited(RATE_LIMIT_BUDGET, || async { Err::<(), _>(limited(None)) })
            .await
            .unwrap_err();
        assert!(started.elapsed() <= RATE_LIMIT_BUDGET);
        let text = err.to_string();
        assert!(
            text.starts_with("the backend still answered HTTP 429 after ")
                && text.ends_with(" s of retries: slow down"),
            "{text}"
        );
    }

    fn server_error(status: u16) -> anyhow::Error {
        RetryableHttpError {
            status,
            retry_after: None,
            message: format!("Groq API error ({status}): error code: {status}"),
        }
        .into()
    }

    /// A server that keeps failing is retried three times, 5, 15 and 30 s
    /// apart, then given up on with its own message, as a final error.
    #[tokio::test(start_paused = true)]
    async fn a_lasting_server_error_gives_up_after_a_few_tries() {
        for status in [500, 502, 504, 520, 524] {
            let calls = AtomicUsize::new(0);
            let started = tokio::time::Instant::now();
            let err = retry_rate_limited(RATE_LIMIT_BUDGET, || {
                calls.fetch_add(1, Ordering::Relaxed);
                async move { Err::<(), _>(server_error(status)) }
            })
            .await
            .unwrap_err();
            assert_eq!(calls.load(Ordering::Relaxed), 4, "{status}");
            assert_eq!(started.elapsed(), Duration::from_secs(50), "{status}");
            assert_eq!(
                err.to_string(),
                format!(
                    "the backend still answered HTTP {status} after 50 s of retries: \
                     Groq API error ({status}): error code: {status}"
                )
            );
            assert!(err.downcast_ref::<RetryableHttpError>().is_none());
            assert!(err.downcast_ref::<RateLimitExhausted>().is_some());
        }
    }

    #[test]
    fn server_error_waits_follow_the_schedule_and_the_budget() {
        let budget = RATE_LIMIT_BUDGET;
        let at = |retries, retry_after, waited| {
            server_error_delay(&SERVER_ERROR_BACKOFF, retries, retry_after, waited, budget)
        };
        assert_eq!(at(0, None, Duration::ZERO), Some(Duration::from_secs(5)));
        assert_eq!(at(2, None, Duration::ZERO), Some(Duration::from_secs(30)));
        assert_eq!(at(3, None, Duration::ZERO), None, "the schedule ran out");
        // The server's own wait wins while the schedule lasts.
        assert_eq!(
            at(1, Some(Duration::from_secs(40)), Duration::ZERO),
            Some(Duration::from_secs(40))
        );
        assert_eq!(at(0, None, Duration::from_secs(297)), None, "budget");
    }

    /// One request to `addr`: a small POST whose answer is read as text,
    /// the way the backends send a chunk.
    async fn post(client: &reqwest::Client, url: &str) -> anyhow::Result<String> {
        let response = client.post(url).body("chunk").send().await?;
        Ok(response.text().await?)
    }

    /// Accept one connection, read the request head, then run `then` on it.
    async fn accept_then<F, Fut>(listener: tokio::net::TcpListener, then: F)
    where
        F: FnOnce(tokio::net::TcpStream) -> Fut,
        Fut: Future<Output = ()>,
    {
        use tokio::io::AsyncReadExt;
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = stream.read(&mut buf).await;
        then(stream).await;
    }

    /// A URL that refuses connections while the returned socket lives.
    fn refused_url() -> (tokio::net::TcpSocket, String) {
        let (closed, addr) = crate::transcription::test_server::refused_addr();
        (closed, format!("http://{addr}/transcriptions"))
    }

    async fn local_listener() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/transcriptions", listener.local_addr().unwrap());
        (listener, url)
    }

    /// A resolver that fails every lookup, so a DNS failure needs no DNS.
    struct NoSuchHost;

    impl reqwest::dns::Resolve for NoSuchHost {
        fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            Box::pin(async { Err("failed to lookup address information".into()) })
        }
    }

    /// Each transport failure a chunk upload can meet is classified as one,
    /// through anyhow context layers too; an answer never is.
    #[tokio::test]
    async fn transport_errors_are_classified() {
        use tokio::io::AsyncWriteExt;
        let client = reqwest::Client::new();
        let is_transport = |err: anyhow::Error| {
            let wrapped = err.context("failed to transcribe the mic track");
            is_transport_error(&wrapped)
        };

        // Connection refused: nothing listens on the port.
        let (_closed, url) = refused_url();
        let err = post(&client, &url).await.unwrap_err();
        assert!(is_transport(err), "refused");

        // Connection reset while waiting for the answer.
        let (listener, url) = local_listener().await;
        let server = tokio::spawn(accept_then(listener, |stream| async move {
            stream.set_zero_linger().unwrap();
            drop(stream);
        }));
        let err = post(&client, &url).await.unwrap_err();
        server.await.unwrap();
        assert!(is_transport(err), "reset");

        // A server that accepts and never answers: the request times out.
        let (listener, url) = local_listener().await;
        let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(accept_then(listener, |stream| async move {
            let _ = hold_rx.await;
            drop(stream);
        }));
        let impatient = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let err = post(&impatient, &url).await.unwrap_err();
        drop(hold_tx);
        server.await.unwrap();
        assert!(is_transport(err), "timeout");

        // The answer breaks off: fewer body bytes than its content-length.
        let (listener, url) = local_listener().await;
        let server = tokio::spawn(accept_then(listener, |mut stream| async move {
            let head = "HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"text\": ";
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        }));
        let err = post(&client, &url).await.unwrap_err();
        server.await.unwrap();
        assert!(is_transport(err), "body read");

        // TLS handshake failure: an https URL on a plain HTTP server.
        let (listener, url) = local_listener().await;
        let url = url.replace("http://", "https://");
        let server = tokio::spawn(accept_then(listener, |mut stream| async move {
            let _ = stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                .await;
        }));
        let err = post(&client, &url).await.unwrap_err();
        server.await.unwrap();
        assert!(is_transport(err), "TLS handshake");

        // DNS failure.
        let resolver = reqwest::Client::builder()
            .dns_resolver(std::sync::Arc::new(NoSuchHost))
            .build()
            .unwrap();
        let err = post(&resolver, "http://api.whisrs.invalid/transcriptions")
            .await
            .unwrap_err();
        assert!(is_transport(err), "DNS");

        // Answers are not transport errors: a whole body that does not
        // parse, and any HTTP status, a retryable one included.
        let (listener, url) = local_listener().await;
        let server = tokio::spawn(accept_then(listener, |mut stream| async move {
            let answer =
                "HTTP/1.1 200 OK\r\ncontent-length: 8\r\nconnection: close\r\n\r\nnot json";
            stream.write_all(answer.as_bytes()).await.unwrap();
        }));
        let response = client.post(&url).body("chunk").send().await.unwrap();
        let err = response.json::<serde_json::Value>().await.unwrap_err();
        server.await.unwrap();
        assert!(!is_transport(err.into()), "a body that does not parse");
        for status in [400, 401, 404, 413, 500, 524] {
            let err = super::super::http_error(status, None, format!("API error ({status})"));
            assert!(!is_transport(err), "HTTP {status}");
        }
        assert!(!is_transport(anyhow::anyhow!("401 unauthorized")));
    }

    /// Only the other end closing the connection counts as a crash: a
    /// timeout or an unreachable host is the network, and a refused
    /// connection a server that is not up yet.
    #[test]
    fn only_a_peer_close_is_a_crash() {
        use std::io::{Error, ErrorKind};
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(closed_by_peer(&Error::from(kind)), "{kind:?}");
        }
        for kind in [
            ErrorKind::TimedOut,
            ErrorKind::HostUnreachable,
            ErrorKind::NetworkUnreachable,
            ErrorKind::NetworkDown,
            ErrorKind::ConnectionRefused,
        ] {
            assert!(!closed_by_peer(&Error::from(kind)), "{kind:?}");
        }

        /// Shaped like reqwest's chain: an outer error over the cause.
        #[derive(Debug)]
        struct Outer(Box<dyn std::error::Error + 'static>);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("error sending request")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&*self.0)
            }
        }
        let reset = Outer(Box::new(Error::from(ErrorKind::ConnectionReset)));
        assert!(closed_by_peer(&reset), "found under the outer error");
        let quiet = Outer(Box::new(Error::from(ErrorKind::TimedOut)));
        assert!(!closed_by_peer(&quiet));
    }

    /// Short stand-ins for the network waits, so these tests talk to a
    /// real socket without sleeping 110 s.
    const QUICK_NETWORK: [Duration; 4] = [Duration::from_millis(1); 4];

    /// A connection reset on the first try is retried, and the chunk goes
    /// through on the next one.
    #[tokio::test]
    async fn a_network_error_is_retried_until_it_goes_through() {
        use tokio::io::AsyncWriteExt;
        let (listener, url) = local_listener().await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            stream.set_zero_linger().unwrap();
            drop(stream);
            accept_then(listener, |mut stream| async move {
                let answer = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
                stream.write_all(answer.as_bytes()).await.unwrap();
            })
            .await;
        });
        let client = reqwest::Client::new();
        let calls = AtomicUsize::new(0);
        let text = retry_with_backoff(RATE_LIMIT_BUDGET, &[], &QUICK_NETWORK, || {
            calls.fetch_add(1, Ordering::Relaxed);
            post(&client, &url)
        })
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(text, "ok");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    /// A network that stays down is tried once and four more times, then
    /// given up on as a [`NetworkUnavailable`], which an outer layer does
    /// not retry again.
    #[tokio::test]
    async fn a_lasting_network_error_ends_in_network_unavailable() {
        let (_closed, url) = refused_url();
        let client = reqwest::Client::new();
        let calls = AtomicUsize::new(0);
        let err = retry_with_backoff(RATE_LIMIT_BUDGET, &[], &QUICK_NETWORK, || {
            calls.fetch_add(1, Ordering::Relaxed);
            post(&client, &url)
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::Relaxed), 5);
        assert!(is_network_unavailable(&err));
        assert!(!is_transport_error(&err), "final for an outer layer");
        assert!(err.downcast_ref::<RateLimitExhausted>().is_none());
        let text = err.to_string();
        assert!(
            text.starts_with("could not reach the transcription service after 0 s of retries: ")
                && text.contains("refused"),
            "{text}"
        );

        let calls = AtomicUsize::new(0);
        let outer = retry_rate_limited(RATE_LIMIT_BUDGET, || {
            calls.fetch_add(1, Ordering::Relaxed);
            let err = NetworkUnavailable {
                message: "could not reach the transcription service".into(),
            };
            async move { Err::<(), _>(anyhow::Error::from(err)) }
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(is_network_unavailable(&outer));
    }

    #[test]
    fn network_waits_follow_their_schedule() {
        let at = |retries, waited| {
            server_error_delay(
                &NETWORK_ERROR_BACKOFF,
                retries,
                None,
                waited,
                RATE_LIMIT_BUDGET,
            )
        };
        let waits: Vec<_> = (0..4).map(|n| at(n, Duration::ZERO).unwrap()).collect();
        assert_eq!(
            waits,
            [5, 15, 30, 60].map(Duration::from_secs),
            "5, 15, 30 and 60 s"
        );
        assert_eq!(at(4, Duration::ZERO), None, "the schedule ran out");
        assert_eq!(at(0, Duration::from_secs(296)), None, "the budget");
    }

    #[tokio::test(start_paused = true)]
    async fn other_errors_are_not_retried() {
        let calls = AtomicUsize::new(0);
        let err = retry_rate_limited(RATE_LIMIT_BUDGET, || {
            calls.fetch_add(1, Ordering::Relaxed);
            async { Err::<(), _>(anyhow::anyhow!("401 unauthorized")) }
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(err.to_string(), "401 unauthorized");
    }
}
