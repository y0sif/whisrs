//! One-shot HTTP server for backend tests: what a request put on the wire.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket};

/// An address that refuses connections while the returned socket lives: a
/// port bound and never listened on. Bind it as `let (_closed, addr) =
/// refused_addr();` (a bare `_` drops it at once). A listener bound and
/// dropped instead frees its port, which a test running in parallel can be
/// handed, so a connection meant to be refused reaches that test's server.
pub(crate) fn refused_addr() -> (TcpSocket, std::net::SocketAddr) {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = socket.local_addr().unwrap();
    (socket, addr)
}

/// A request as the server read it.
pub(crate) struct Request {
    /// Request line and headers, through the blank line.
    pub head: String,
    pub body: Vec<u8>,
}

impl Request {
    /// The value of header `name`, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    /// The headers of the multipart part named `file` and the bytes from
    /// the start of its data to the end of the body.
    pub fn file_part(&self) -> (String, &[u8]) {
        let marker = b"name=\"file\"";
        let start = self
            .body
            .windows(marker.len())
            .position(|w| w == marker)
            .expect("multipart body has no `file` part");
        let data = start
            + self.body[start..]
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("file part has no header end")
            + 4;
        let headers = String::from_utf8_lossy(&self.body[start..data]).into_owned();
        (headers, &self.body[data..])
    }

    /// The multipart text fields, in order, as `(name, value)`: every part
    /// but a file.
    pub fn text_fields(&self) -> Vec<(String, String)> {
        let content_type = self
            .header("content-type")
            .expect("request has no content-type");
        let boundary = content_type
            .split_once("boundary=")
            .expect("content-type has no multipart boundary")
            .1
            .trim_matches('"');
        let body = String::from_utf8_lossy(&self.body);
        body.split(&format!("--{boundary}"))
            .filter_map(|part| {
                let (headers, value) = part.split_once("\r\n\r\n")?;
                if headers.contains("filename=") {
                    return None;
                }
                let name = headers.split_once("name=\"")?.1.split('"').next()?;
                let value = value.strip_suffix("\r\n").unwrap_or(value);
                Some((name.to_string(), value.to_string()))
            })
            .collect()
    }
}

/// Accept a single connection, read the whole request, answer `200 OK` with
/// `body`, and hand back the request so a test can assert on what was
/// actually put on the wire.
pub(crate) async fn serve_once(listener: TcpListener, body: &'static str) -> Request {
    serve_one(&listener, 200, body).await
}

/// Answer one connection per `(status, body)`, in order, and hand back the
/// requests: a server that fails before it succeeds, or keeps failing.
pub(crate) async fn serve_responses(
    listener: TcpListener,
    responses: Vec<(u16, &'static str)>,
) -> Vec<Request> {
    let mut requests = Vec::new();
    for (status, body) in responses {
        requests.push(serve_one(&listener, status, body).await);
    }
    requests
}

/// Accept one connection, read the whole request, and close the connection
/// without an answer: a server that crashed handling the request.
pub(crate) async fn crash_after_request(listener: &TcpListener) -> Request {
    accept_request(listener).await.1
}

/// Accept one connection and read the whole request, leaving the
/// connection open and unanswered for the caller to hold or drop.
pub(crate) async fn accept_request(listener: &TcpListener) -> (tokio::net::TcpStream, Request) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let request = read_request(&mut stream).await;
    (stream, request)
}

/// Read one request and answer it with `status` and `body`, closing the
/// connection so the client opens a new one for its next request.
async fn serve_one(listener: &TcpListener, status: u16, body: &str) -> Request {
    let (mut stream, _) = listener.accept().await.unwrap();
    let request = read_request(&mut stream).await;
    let reason = if status == 200 { "OK" } else { "Error" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    request
}

/// Read a request's head and its whole body.
async fn read_request(stream: &mut tokio::net::TcpStream) -> Request {
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

    // Drain the body first: replying while the client is still writing
    // surfaces as a connection reset instead of a parsed response.
    let content_length: usize = head
        .to_ascii_lowercase()
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .expect("reqwest must send content-length for a body of known length");
    while buf.len() < head_end + content_length {
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Request {
        head,
        body: buf.split_off(head_end),
    }
}
