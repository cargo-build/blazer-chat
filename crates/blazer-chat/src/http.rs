//! Minimal HTTP/1.1 client on compio sockets — the transport layer under the
//! Rocket.Chat protocol client: request line, headers, one content-length or
//! chunked body, connection closed per request. [`Transport`] is the seam the
//! protocol layer depends on; [`HttpTransport`] is the compio implementation
//! (plain TCP for `http://`, TLS via native-tls for `https://`), and
//! protocol tests substitute their own.

use std::io::{ErrorKind, Read as _};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
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
        let head = request_head(method, path, &self.host, content_type, headers, body.len())?;
        conn.write_all(head.into_bytes()).await?;
        conn.write_all(body.to_vec()).await?;
        conn.read_bounded(MAX_RESPONSE_BYTES).await
    }
}

/// Any single response is capped here: legitimate payloads (history
/// pages, attachments) sit far below, and a hostile server must not
/// grow the process without bound (audit sec-network MF3).
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Builds the request head, refusing control characters in anything the
/// server or session supplied: a CR/LF in the path, host or a header
/// value would inject request lines or headers (audit sec-flow #2 /
/// sec-network MF2). Header names come only from our code, but a stray
/// colon would still split a header.
fn request_head(
    method: &str,
    path: &str,
    host: &str,
    content_type: &str,
    headers: &[(String, String)],
    body_len: usize,
) -> std::io::Result<String> {
    let control = |value: &str| value.contains(['\r', '\n', '\0']);
    if [method, path, host, content_type].into_iter().any(control) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "request line carries control characters",
        ));
    }
    for (name, value) in headers {
        if control(name) || control(value) || name.contains(':') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "header carries control characters or a split colon",
            ));
        }
    }
    let mut head = format!(
        "{} {} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\ncontent-type: {}\r\n",
        method, path, host, content_type
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str(&format!("content-length: {}\r\n\r\n", body_len));
    Ok(head)
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
        self.get_bytes_with_headers(path, &[]).await
    }

    /// Binary GET with explicit headers (auth for file downloads). The
    /// server force-gzips file responses (ignoring `Accept-Encoding`),
    /// so a `content-encoding: gzip` body is decoded here.
    pub async fn get_bytes_with_headers(
        &self,
        path: &str,
        headers: &[(String, String)],
    ) -> std::io::Result<Vec<u8>> {
        let raw = self
            .exchange("GET", path, headers, "application/json", b"")
            .await?;
        let wire = parse_wire(&raw)?;
        if !(200..300).contains(&wire.status) {
            return Err(std::io::Error::other(format!(
                "binary get http {}",
                wire.status
            )));
        }
        let gzipped = wire
            .head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .any(|(name, value)| {
                name.trim().eq_ignore_ascii_case("content-encoding")
                    && value.trim().eq_ignore_ascii_case("gzip")
            });
        if !gzipped {
            return Ok(wire.body);
        }
        decode_gzip_capped(&wire.body)
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

    /// Reads to EOF through a bounded buffer: `read_to_end` grew the
    /// vector without limit and even swallowed IO errors (`Ok(raw)`
    /// unconditionally).
    async fn read_bounded(&mut self, cap: usize) -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(64 * 1024);
        loop {
            let chunk = Vec::with_capacity(16 * 1024);
            let BufResult(read, filled) = match self {
                Self::Plain(stream) => stream.read(chunk).await,
                Self::Tls(stream) => stream.read(chunk).await,
            };
            read?;
            if filled.is_empty() {
                return Ok(buf);
            }
            if buf.len() + filled.len() > cap {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "response exceeds the 64 MiB cap",
                ));
            }
            buf.extend_from_slice(&filled);
        }
    }
}

/// Inflates a gzip body under [`MAX_RESPONSE_BYTES`]: a decompression
/// bomb must not grow the process without bound (audit sec-network
/// MF3). The cap is checked after a bounded `take`, so a bomb that
/// would exceed it fails instead of truncating silently.
fn decode_gzip_capped(body: &[u8]) -> std::io::Result<Vec<u8>> {
    // The Vec grows only as far as the decoder's take() allows.
    let mut decoded = Vec::with_capacity(body.len().saturating_mul(4));
    flate2::read::GzDecoder::new(body)
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut decoded)
        .map_err(|error| std::io::Error::other(format!("gzip decode: {error}")))?;
    if decoded.len() > MAX_RESPONSE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "gzip body exceeds the 64 MiB cap",
        ));
    }
    Ok(decoded)
}

/// Wire framing result: status, the decoded body as raw bytes (binary
/// safe — lossy UTF-8 is a JSON-layer concern), and the header block for
/// content sniffing.
struct WireResponse {
    status: u16,
    body: Vec<u8>,
    head: String,
}

/// Splits head/body, decodes content-length or chunked bodies, and
/// reports the status code — all on raw bytes: content-length counts
/// bytes, and slicing a lossy-UTF-8 string at byte offsets panics on
/// multibyte input. Truncated reads surface as InvalidData, not empty
/// bodies.
fn parse_wire(raw: &[u8]) -> std::io::Result<WireResponse> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| invalid("no HTTP header/body separator"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let rest = &raw[split + 4..];

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
        decode_chunked(rest).ok_or_else(|| invalid("malformed chunked body"))?
    } else if let Some(length) = content_length {
        if rest.len() < length {
            return Err(invalid("truncated body"));
        }
        rest[..length].to_vec()
    } else {
        rest.to_vec()
    };

    Ok(WireResponse {
        status,
        body,
        head: head.into_owned(),
    })
}

/// The JSON layer over [`parse_wire`]: bodies are text.
fn parse_response(raw: &[u8]) -> std::io::Result<Response> {
    let wire = parse_wire(raw)?;
    Ok(Response {
        status: wire.status,
        body: String::from_utf8_lossy(&wire.body).into_owned(),
    })
}

/// Decodes `length\r\n<bytes>\r\n` chunks until the zero chunk.
fn decode_chunked(body: &[u8]) -> Option<Vec<u8>> {
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
            return Some(out);
        }
        let chunk_start = line_end + 2;
        // `size` is server-controlled: bound it by the remaining bytes
        // instead of letting `chunk_start + size` overflow (a huge hex
        // chunk line used to wrap the arithmetic into a panicking slice).
        let available = rest.len().checked_sub(chunk_start)?;
        if size > available {
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
    use std::io::Write as _;

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

    #[test]
    fn decode_chunked_rejects_an_overflowing_chunk_size() {
        // A server-controlled `FFFFFFFFFFFFFFFF` chunk line must be
        // rejected, not wrap the arithmetic into a panicking slice.
        assert_eq!(decode_chunked(b"FFFFFFFFFFFFFFFF\r\nx"), None);
        assert_eq!(
            decode_chunked(b"5\r\nhello\r\n0\r\n\r\n"),
            Some(b"hello".to_vec())
        );
    }

    #[test]
    fn binary_get_decodes_chunked_gzip_body() {
        // Real-server case: file downloads arrive chunked AND force-gzipped.
        let payload = b"png-bytes-0123456789";
        let mut gzipped = Vec::new();
        flate2::write::GzEncoder::new(&mut gzipped, flate2::Compression::default())
            .write_all(payload)
            .unwrap();
        let mut framed = format!("{:x}\r\n", gzipped.len()).into_bytes();
        framed.extend_from_slice(&gzipped);
        framed.extend_from_slice(b"\r\n0\r\n\r\n");
        let raw = [
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\ncontent-encoding: gzip\r\n\r\n"
                .as_slice(),
            &framed,
        ]
        .concat();
        let wire = parse_wire(&raw).unwrap();
        assert_eq!(wire.status, 200);
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&wire.body[..])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, payload);
    }

    /// Live roundtrip against a real https server: TCP → TLS → request →
    /// parsed response. Ignored by default; run with
    /// `RCHAT_LIVE_URL=https://host cargo test -- --ignored --nocapture`.
    /// A bad login is the expected outcome (401) — it proves the full path.
    #[test]
    #[ignore = "live network probe"]
    fn live_tls_roundtrip() {
        let url =
            std::env::var("RCHAT_LIVE_URL").expect("set RCHAT_LIVE_URL to the server under test");
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

    #[test]
    fn request_head_refuses_control_characters() {
        assert!(
            request_head(
                "GET",
                "/api/v1/channels.history",
                "host",
                "application/json",
                &[],
                0
            )
            .is_ok()
        );
        // Server-supplied path: CR/LF would inject request lines.
        assert!(
            request_head(
                "GET",
                "/x\r\nHost: evil",
                "host",
                "application/json",
                &[],
                0
            )
            .is_err()
        );
        assert!(request_head("GET", "/x\0", "host", "application/json", &[], 0).is_err());
        // Host and content-type come from settings/server too.
        assert!(
            request_head(
                "GET",
                "/x",
                "host\r\nset-cookie: x",
                "application/json",
                &[],
                0
            )
            .is_err()
        );
        assert!(request_head("GET", "/x", "host", "application/json\r\n", &[], 0).is_err());
        // Header values carry the session token and server strings.
        let headers = vec![("x-token".to_owned(), "v\r\ninject".to_owned())];
        assert!(request_head("GET", "/x", "host", "application/json", &headers, 0).is_err());
        // A colon in the header name would split an extra header.
        let split = vec![("x: inject".to_owned(), "v".to_owned())];
        assert!(request_head("GET", "/x", "host", "application/json", &split, 0).is_err());
    }

    #[test]
    fn gzip_decode_is_capped_against_bombs() {
        // ~65 MiB of zeros compress small but decompress over the cap:
        // decoding must fail instead of growing the process.
        let bomb = {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder
                .write_all(&vec![0u8; MAX_RESPONSE_BYTES + 1024 * 1024])
                .expect("bomb compresses");
            encoder.finish().expect("bomb finishes")
        };
        let Err(error) = decode_gzip_capped(&bomb) else {
            panic!("a bomb over the cap must fail");
        };
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidData,
            "a bomb fails with InvalidData: {error}"
        );
        // An ordinary payload still decodes.
        let plain = {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(b"hello gzip").expect("small compresses");
            encoder.finish().expect("small finishes")
        };
        assert_eq!(
            decode_gzip_capped(&plain).expect("small decodes"),
            b"hello gzip"
        );
    }
}
