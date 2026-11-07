//! Minimal HTTP/1.1 client on compio sockets — the transport layer under the
//! Rocket.Chat protocol client: request line, headers, one content-length or
//! chunked body, connection closed per request. [`Transport`] is the seam the
//! protocol layer depends on; [`HttpTransport`] is the compio implementation
//! (plain TCP for `http://`, TLS via native-tls for `https://`), and
//! protocol tests substitute their own.

use std::io::ErrorKind;

use compio::buf::BufResult;
use compio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use compio::net::TcpStream;
use compio::tls::{TlsConnector, TlsStream};

/// One outbound request.
pub struct Request<'a> {
    pub method: &'a str,
    /// Path including the `/api/v1` prefix, e.g. `/api/v1/login`.
    pub path: &'a str,
    pub headers: Vec<(String, String)>,
    /// Raw bytes: JSON for the API calls, multipart for uploads.
    pub body: Option<Vec<u8>>,
}

pub struct Response {
    pub status: u16,
    pub body: String,
}

/// The protocol layer's view of a network. One method, one error type
/// (`std::io::Error`): non-2xx responses are responses, not errors. The
/// futures stay on the compio thread that spawned them — no `Send`, by
/// design (DECL0001: the driver is thread-local).
pub trait Transport {
    async fn call(&self, request: Request<'_>) -> std::io::Result<Response>;
}

/// The compio implementation: one connection per call, plain TCP or TLS.
pub struct HttpTransport {
    addr: (String, u16),
    host: String,
    tls: bool,
    connector: TlsConnector,
}

impl HttpTransport {
    pub fn new(addr: (String, u16), host: String, tls: bool) -> Self {
        Self {
            addr,
            host,
            tls,
            connector: TlsConnector::from(
                compio::tls::native_tls::TlsConnector::new().expect("native-tls connector builds"),
            ),
        }
    }

    /// Opens one connection — plain TCP, or TLS over TCP.
    async fn connect(&self) -> std::io::Result<Conn> {
        let tcp = TcpStream::connect(self.addr.clone()).await?;
        if self.tls {
            Ok(Conn::Tls(Box::new(
                self.connector.connect(&self.addr.0, tcp).await?,
            )))
        } else {
            Ok(Conn::Plain(tcp))
        }
    }

    /// One request/response exchange at the byte level: builds the head,
    /// writes, and reads the raw response. [`HttpTransport::call`] parses
    /// it into a [`Response`]; [`HttpTransport::get_bytes`] returns the
    /// body bytes (images).
    async fn exchange(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        content_type: &str,
        body: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        let mut conn = self.connect().await?;
        let mut head = format!(
            "{} {} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\ncontent-type: {}\r\n",
            method, path, self.host, content_type
        );
        for (name, value) in headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
        conn.write_all(head.into_bytes()).await?;
        conn.write_all(body.to_vec()).await?;
        conn.read_to_end(Vec::with_capacity(64 * 1024)).await
    }
}

impl Transport for HttpTransport {
    async fn call(&self, req: Request<'_>) -> std::io::Result<Response> {
        // The caller-owned content-type wins (multipart uploads); JSON is
        // the default.
        let content_type = req
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
            .unwrap_or("application/json");
        let raw = self
            .exchange(
                req.method,
                req.path,
                &req.headers,
                content_type,
                req.body.as_deref().unwrap_or_default(),
            )
            .await?;
        parse_response(&raw)
    }
}

impl HttpTransport {
    /// Binary GET for non-JSON resources (emoji images): status must be
    /// 2xx, the body returns as raw bytes.
    pub async fn get_bytes(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let raw = self
            .exchange("GET", path, &[], "application/json", b"")
            .await?;
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or_else(|| std::io::Error::other("no HTTP header/body separator"))?;
        let head = String::from_utf8_lossy(&raw[..split]);
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);
        if !(200..300).contains(&status) {
            return Err(std::io::Error::other(format!("emoji image http {status}")));
        }
        Ok(raw[split + 4..].to_vec())
    }
}

/// One open connection: plain TCP, or TLS over TCP.
enum Conn {
    Plain(TcpStream),
    // Boxed: the TLS session state is over a kilobyte against a 16-byte TCP
    // stream, and clippy rightly flags the enum bloat.
    Tls(Box<TlsStream<TcpStream>>),
}

impl Conn {
    async fn write_all(&mut self, bytes: Vec<u8>) -> std::io::Result<()> {
        let BufResult(written, _) = match self {
            Self::Plain(stream) => stream.write_all(bytes).await,
            Self::Tls(stream) => stream.write_all(bytes).await,
        };
        written?;
        // TLS adapters buffer records; without a flush the request never
        // reaches the wire and the read waits for a response to a phantom
        // request until the server idles out.
        match self {
            Self::Plain(stream) => stream.flush().await,
            Self::Tls(stream) => stream.flush().await,
        }
    }

    async fn read_to_end(&mut self, bytes: Vec<u8>) -> std::io::Result<Vec<u8>> {
        let BufResult(_, raw) = match self {
            Self::Plain(stream) => stream.read_to_end(bytes).await,
            Self::Tls(stream) => stream.read_to_end(bytes).await,
        };
        Ok(raw)
    }
}

/// Splits head/body, decodes content-length or chunked bodies, and reports
/// the status code — all on raw bytes: content-length counts bytes, and
/// slicing a lossy-UTF-8 string at byte offsets panics on multibyte input.
/// Truncated reads surface as InvalidData, not empty bodies.
fn parse_response(raw: &[u8]) -> std::io::Result<Response> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| invalid("no HTTP header/body separator"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];

    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| invalid("unreadable status line"))?;

    let mut content_length = None;
    let mut chunked = false;
    for line in head.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse::<usize>().ok();
            } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }

    let body = if chunked {
        decode_chunked(body).ok_or_else(|| invalid("malformed chunked body"))?
    } else if let Some(length) = content_length {
        if body.len() < length {
            return Err(invalid("truncated body"));
        }
        String::from_utf8_lossy(&body[..length]).into_owned()
    } else {
        String::from_utf8_lossy(body).into_owned()
    };

    Ok(Response { status, body })
}

/// Decodes `length\r\n<bytes>\r\n` chunks until the zero chunk.
fn decode_chunked(body: &[u8]) -> Option<String> {
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    loop {
        let line_end = rest.windows(2).position(|window| window == b"\r\n")?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&rest[..line_end])
                .ok()?
                .split(';')
                .next()?
                .trim(),
            16,
        )
        .ok()?;
        if size == 0 {
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
        let chunk_start = line_end + 2;
        if rest.len() < chunk_start + size {
            return None;
        }
        out.extend_from_slice(&rest[chunk_start..chunk_start + size]);
        rest = &rest[chunk_start + size..];
        rest = rest.strip_prefix(b"\r\n").unwrap_or(rest);
    }
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(ErrorKind::InvalidData, message.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"ok\":true}");
    }

    #[test]
    fn parses_multibyte_body_at_byte_length() {
        // Real-server case: JSON carries Cyrillic; content-length counts
        // bytes, and a lossy-string slice would split a multibyte char.
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\n{\"p\":\"\xd0\xbf\"}";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.body, "{\"p\":\"\u{43f}\"}");
    }

    #[test]
    fn parses_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\n{\"ok\r\n7\r\n\":true}\r\n0\r\n\r\n";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"ok\":true}");
    }

    #[test]
    fn rejects_truncated_body() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort";
        assert!(parse_response(raw).is_err());
    }

    /// Live roundtrip against a real https server: TCP → TLS → request →
    /// parsed response. Ignored by default; run with
    /// `RCHAT_LIVE_URL=https://host cargo test -- --ignored --nocapture`.
    /// A bad login is the expected outcome (401) — it proves the full path.
    #[test]
    #[ignore = "live network probe"]
    fn live_tls_roundtrip() {
        let url = std::env::var("RCHAT_LIVE_URL")
            .unwrap_or_else(|_| "https://rocketchat-student.21-school.ru".to_owned());
        let settings = crate::rc::RcSettings::parse(&url).unwrap();
        let transport =
            HttpTransport::new(settings.addr(), settings.host_header(), settings.is_tls());
        println!(
            "connecting to {:?} (tls={})",
            settings.addr(),
            settings.is_tls()
        );
        let request = Request {
            method: "POST",
            path: "/api/v1/login",
            headers: Vec::new(),
            body: Some(br#"{"user":"probe","password":"probe"}"#.to_vec()),
        };
        let response = compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(transport.call(request))
            .expect("live TLS roundtrip completes");
        println!(
            "status={} body={}",
            response.status,
            &response.body[..response.body.len().min(120)]
        );
        assert!(response.status == 400 || response.status == 401);
    }
}
