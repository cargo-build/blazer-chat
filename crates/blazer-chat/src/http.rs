//! Minimal HTTP/1.1 client on compio sockets — the transport layer under the
//! Rocket.Chat protocol client: request line, headers, framed
//! content-length or chunked bodies over one persistent connection
//! (m10 keep-alive). [`Transport`] is the seam the protocol layer depends
//! on; [`HttpTransport`] is the compio implementation (plain TCP for
//! `http://`, TLS via native-tls for `https://`), and protocol tests
//! substitute their own.
//!
//! Keep-alive rules: responses are consumed by framing, never by EOF —
//! surplus bytes would corrupt the next exchange, so a pooled connection
//! carrying unread bytes is dropped. A reused connection that fails
//! before a readable response is the server idle-close race: idempotent
//! methods (GET/HEAD) retry once on a fresh socket, POST/DELETE surface
//! the error (a blind retry could double-send a message).

use std::cell::RefCell;
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

    /// Binary GET for non-JSON resources (emoji images, avatars,
    /// attachments): status must be 2xx, raw bytes out, gzip decoded.
    async fn get_bytes(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.get_bytes_with_headers(path, &[]).await
    }

    /// Binary GET with explicit headers (auth for file downloads).
    async fn get_bytes_with_headers(
        &self,
        path: &str,
        headers: &[(String, String)],
    ) -> std::io::Result<Vec<u8>>;
}

/// The compio implementation: one persistent connection, plain TCP or TLS.
pub struct HttpTransport {
    addr: (String, u16),
    host: String,
    tls: bool,
    connector: TlsConnector,
    /// The idle keep-alive connection, if the server left one open. The
    /// transport lives on the compio thread alone, so a `RefCell` pool of
    /// one is the whole story (no `Send`, per DECL0001).
    pooled: RefCell<Option<Pooled>>,
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
            pooled: RefCell::new(None),
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

    /// One request/response exchange: reuses the idle connection when the
    /// server left one open, reconnects otherwise. A reused connection
    /// that fails before a readable response is the server idle-close
    /// race — idempotent methods retry once on a fresh socket.
    async fn exchange(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        content_type: &str,
        body: &[u8],
    ) -> std::io::Result<WireResponse> {
        let head = request_head(method, path, &self.host, content_type, headers, body.len())?;
        let idempotent = matches!(method, "GET" | "HEAD");
        let mut retried = false;
        loop {
            let reused = self.pooled.borrow().is_some();
            let taken = self.pooled.borrow_mut().take();
            let mut pooled = match taken {
                // Surplus bytes from a previous response mean the framing
                // consumed wrong or the server pipelined — protocol
                // confusion: drop the socket and go fresh.
                Some(pooled) if pooled.buf.is_empty() => pooled,
                _ => Pooled::fresh(self.connect().await?),
            };
            match self.exchange_on(&mut pooled, &head, body).await {
                Ok((response, reusable)) => {
                    if reusable {
                        *self.pooled.borrow_mut() = Some(pooled);
                    }
                    return Ok(response);
                }
                Err(error) if reused && idempotent && !retried => {
                    retried = true;
                    eprintln!(
                        "blazer-chat: {method} {path} failed on a reused connection, retrying fresh: {error}"
                    );
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Writes one prepared head + body, then reads the framed response.
    async fn exchange_on(
        &self,
        pooled: &mut Pooled,
        head: &str,
        body: &[u8],
    ) -> std::io::Result<(WireResponse, bool)> {
        pooled.stream.write_all(head.as_bytes().to_vec()).await?;
        pooled.stream.write_all(body.to_vec()).await?;
        read_framed(pooled).await
    }
}

/// Any single response is capped here: legitimate payloads (history
/// pages, attachments) sit far below, and a hostile server must not
/// grow the process without bound (audit sec-network MF3).
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Head blocks are small; a runaway head with no terminator fails well
/// before the body cap can matter.
const MAX_HEAD_BYTES: usize = 64 * 1024;

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
        "{} {} HTTP/1.1\r\nhost: {}\r\nconnection: keep-alive\r\ncontent-type: {}\r\n",
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
    async fn get_bytes(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.get_bytes_with_headers(path, &[]).await
    }

    async fn get_bytes_with_headers(
        &self,
        path: &str,
        headers: &[(String, String)],
    ) -> std::io::Result<Vec<u8>> {
        // The inherent impl carries the exchange/gzip logic.
        HttpTransport::get_bytes_with_headers(self, path, headers).await
    }

    async fn call(&self, req: Request<'_>) -> std::io::Result<Response> {
        // The caller-owned content-type wins (multipart uploads); JSON is
        // the default.
        let content_type = req
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
            .unwrap_or("application/json");
        let wire = self
            .exchange(
                req.method,
                req.path,
                &req.headers,
                content_type,
                req.body.as_deref().unwrap_or_default(),
            )
            .await?;
        Ok(Response {
            status: wire.status,
            body: String::from_utf8_lossy(&wire.body).into_owned(),
        })
    }
}

impl HttpTransport {
    /// Binary GET with explicit headers (auth for file downloads). The
    /// server force-gzips file responses (ignoring `Accept-Encoding`),
    /// so a `content-encoding: gzip` body is decoded here.
    pub async fn get_bytes_with_headers(
        &self,
        path: &str,
        headers: &[(String, String)],
    ) -> std::io::Result<Vec<u8>> {
        let wire = self
            .exchange("GET", path, headers, "application/json", b"")
            .await?;
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

/// One pooled connection with its unread byte buffer. Bytes past the
/// consumed response stay here for the next exchange only until the
/// next request is built — any leftover then means the framing consumed
/// wrong, and the whole connection is dropped.
struct Pooled {
    stream: Conn,
    buf: Vec<u8>,
}

impl Pooled {
    fn fresh(stream: Conn) -> Self {
        Self {
            stream,
            buf: Vec::new(),
        }
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

    /// One bounded read: an empty buffer means the peer closed (EOF).
    async fn read_chunk(&mut self) -> std::io::Result<Vec<u8>> {
        let chunk = Vec::with_capacity(16 * 1024);
        let BufResult(read, filled) = match self {
            Self::Plain(stream) => stream.read(chunk).await,
            Self::Tls(stream) => stream.read(chunk).await,
        };
        read?;
        Ok(filled)
    }
}

/// Reads one framed response, leaving surplus bytes in `pooled.buf` for
/// the next exchange. EOF resolves only a header-delimited response
/// (no framing, body ends at the close); a framed body cut at EOF is
/// truncated and fails.
async fn read_framed(pooled: &mut Pooled) -> std::io::Result<(WireResponse, bool)> {
    loop {
        match parse_framed(&pooled.buf) {
            Parsed::Done {
                response,
                consumed,
                reusable,
            } => {
                pooled.buf.drain(..consumed);
                return Ok((response, reusable));
            }
            Parsed::Invalid(error) => return Err(error),
            Parsed::NeedMore => {}
        }
        if pooled.buf.len() > MAX_RESPONSE_BYTES + MAX_HEAD_BYTES {
            return Err(invalid("response exceeds the 64 MiB cap"));
        }
        let chunk = pooled.stream.read_chunk().await?;
        if chunk.is_empty() {
            return response_at_close(&mut pooled.buf).map(|response| (response, false));
        }
        pooled.buf.extend_from_slice(&chunk);
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

/// How the body length is determined: explicit byte count, chunked
/// transfer-coding, or read-to-close (HTTP/1.0-style). Per RFC 9112
/// §6.1 chunked wins when both headers appear.
enum Framing {
    Length(usize),
    Chunked,
    UntilClose,
}

struct HeadInfo {
    status: u16,
    framing: Framing,
    /// HTTP/1.1 without `connection: close` — the connection may serve
    /// the next request.
    reusable: bool,
}

/// Parses the status line and the framing headers of a complete head
/// block (terminator not included). An unparsable content-length keeps
/// the read-to-close fallback (the pre-keep-alive wire behavior).
fn parse_head(head: &str) -> std::io::Result<HeadInfo> {
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| invalid("unreadable status line"))?;
    let mut framing = Framing::UntilClose;
    let mut close = false;
    for line in head.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" if matches!(framing, Framing::UntilClose) => {
                if let Ok(length) = value.parse::<usize>() {
                    framing = Framing::Length(length);
                }
            }
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                framing = Framing::Chunked;
            }
            "connection" if value.to_ascii_lowercase().contains("close") => close = true,
            _ => {}
        }
    }
    let reusable = head
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http/1.1"))
        && !close;
    Ok(HeadInfo {
        status,
        framing,
        reusable,
    })
}

/// The outcome of one framing attempt over the buffered bytes.
enum Parsed {
    Done {
        response: WireResponse,
        /// Bytes of `buf` belonging to this response — the next exchange
        /// must not see them.
        consumed: usize,
        reusable: bool,
    },
    /// The buffer holds no complete response yet; read more.
    NeedMore,
    Invalid(std::io::Error),
}

/// Attempts to parse one complete response from the head of `buf`.
fn parse_framed(buf: &[u8]) -> Parsed {
    let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Parsed::NeedMore;
    };
    // A complete head is bounded on its own: a hostile peer must not
    // grow the buffered head beyond the cap before failing (audit
    // A.8 — MAX_HEAD_BYTES is a real head bound on both the keep-alive
    // and the close path). A stream that never terminates still grows
    // to the combined MAX_RESPONSE_BYTES + MAX_HEAD_BYTES bound below.
    if split > MAX_HEAD_BYTES {
        return Parsed::Invalid(invalid("response head exceeds the 64 KiB cap"));
    }
    let head = String::from_utf8_lossy(&buf[..split]);
    let info = match parse_head(&head) {
        Ok(info) => info,
        Err(error) => return Parsed::Invalid(error),
    };
    let rest = &buf[split + 4..];
    match info.framing {
        Framing::Length(length) => {
            if length > MAX_RESPONSE_BYTES {
                return Parsed::Invalid(invalid("content-length exceeds the 64 MiB cap"));
            }
            if rest.len() < length {
                return Parsed::NeedMore;
            }
            Parsed::Done {
                response: WireResponse {
                    status: info.status,
                    body: rest[..length].to_vec(),
                    head: head.into_owned(),
                },
                consumed: split + 4 + length,
                reusable: info.reusable,
            }
        }
        Framing::Chunked => match decode_chunked_prefix(rest) {
            ChunkedStep::Done { body, consumed } => Parsed::Done {
                response: WireResponse {
                    status: info.status,
                    body,
                    head: head.into_owned(),
                },
                consumed: split + 4 + consumed,
                reusable: info.reusable,
            },
            ChunkedStep::NeedMore => Parsed::NeedMore,
            ChunkedStep::Invalid(message) => Parsed::Invalid(invalid(message)),
        },
        // The length is unknowable until the peer closes.
        Framing::UntilClose => Parsed::NeedMore,
    }
}

/// The connection closed: only a header-delimited response is legitimate
/// here; framed bodies cut at EOF are truncated and fail.
fn response_at_close(buf: &mut Vec<u8>) -> std::io::Result<WireResponse> {
    let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Err(invalid("connection closed before a response head arrived"));
    };
    // Same head bound as the keep-alive path (state-audit r2 SF-1):
    // EOF resolves the parse, but the head itself stays capped.
    if split > MAX_HEAD_BYTES {
        return Err(invalid("response head exceeds the 64 KiB cap"));
    }
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let info = parse_head(&head)?;
    let rest = buf[split + 4..].to_vec();
    let (body, status) = match info.framing {
        Framing::UntilClose => (rest, info.status),
        Framing::Length(length) if rest.len() >= length => (rest[..length].to_vec(), info.status),
        Framing::Chunked => match decode_chunked_prefix(&rest) {
            ChunkedStep::Done { body, .. } => (body, info.status),
            _ => return Err(invalid("connection closed mid-response")),
        },
        _ => return Err(invalid("connection closed mid-response")),
    };
    buf.clear();
    Ok(WireResponse { status, body, head })
}

/// The outcome of one incremental chunked-decode attempt.
enum ChunkedStep {
    Done {
        body: Vec<u8>,
        /// Bytes of the input belonging to this response, including the
        /// zero chunk and any trailer block — the stream is positioned
        /// at the next response after them.
        consumed: usize,
    },
    NeedMore,
    Invalid(&'static str),
}

/// Trailer blocks are bounded: a hostile zero-chunk with endless trailer
/// lines must fail instead of buffering forever.
const MAX_TRAILER_BYTES: usize = 16 * 1024;

/// Decodes `length\r\n<bytes>\r\n` chunks until the zero chunk over the
/// head of `body`, consuming trailers through their terminator. A chunk
/// is consumed only once fully present (including its trailing CRLF), so
/// partial reads simply return [`ChunkedStep::NeedMore`].
fn decode_chunked_prefix(body: &[u8]) -> ChunkedStep {
    let mut out = Vec::with_capacity(body.len().min(64 * 1024));
    let mut rest = body;
    let mut trailer_bytes = 0usize;
    loop {
        let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") else {
            return ChunkedStep::NeedMore;
        };
        let size = match std::str::from_utf8(&rest[..line_end])
            .ok()
            .and_then(|line| line.split(';').next())
            .and_then(|line| usize::from_str_radix(line.trim(), 16).ok())
        {
            Some(size) => size,
            None => return ChunkedStep::Invalid("malformed chunk size"),
        };
        if size == 0 {
            // Trailer lines (usually none) end at the first empty line.
            let mut trailers = &rest[line_end + 2..];
            loop {
                let Some(end) = trailers.windows(2).position(|w| w == b"\r\n") else {
                    return ChunkedStep::NeedMore;
                };
                trailer_bytes += end + 2;
                if trailer_bytes > MAX_TRAILER_BYTES {
                    return ChunkedStep::Invalid("chunked trailer block exceeds 16 KiB");
                }
                if end == 0 {
                    // The terminator belongs to this response: consumed
                    // covers everything up to and including it.
                    return ChunkedStep::Done {
                        body: out,
                        consumed: body.len() - trailers.len() + 2,
                    };
                }
                trailers = &trailers[end + 2..];
            }
        }
        let chunk_start = line_end + 2;
        let Some(available) = rest.len().checked_sub(chunk_start) else {
            return ChunkedStep::NeedMore;
        };
        if size > MAX_RESPONSE_BYTES {
            return ChunkedStep::Invalid("chunk size exceeds the 64 MiB cap");
        }
        if out.len() + size > MAX_RESPONSE_BYTES {
            return ChunkedStep::Invalid("chunked body exceeds the 64 MiB cap");
        }
        if available < size {
            return ChunkedStep::NeedMore;
        }
        // `size` is server-controlled: bound it by the remaining bytes
        // instead of letting `chunk_start + size` overflow (a huge hex
        // chunk line used to wrap the arithmetic into a panicking slice).
        if rest.len() < chunk_start + size + 2 {
            return ChunkedStep::NeedMore;
        }
        if &rest[chunk_start + size..chunk_start + size + 2] != b"\r\n" {
            return ChunkedStep::Invalid("chunk data is not CRLF-terminated");
        }
        out.extend_from_slice(&rest[chunk_start..chunk_start + size]);
        rest = &rest[chunk_start + size + 2..];
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // ── framing units ──

    #[test]
    fn parses_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}";
        let parsed = parse_framed(raw);
        let Parsed::Done {
            response,
            consumed,
            reusable,
        } = parsed
        else {
            panic!("a complete content-length response parses");
        };
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"{\"ok\":true}".as_slice());
        assert_eq!(consumed, raw.len());
        assert!(reusable);
    }

    #[test]
    fn parses_multibyte_body_at_byte_length() {
        // Real-server case: JSON carries Cyrillic; content-length counts
        // bytes, and a lossy-string slice would split a multibyte char.
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\n{\"p\":\"\xd0\xbf\"}";
        let Parsed::Done { response, .. } = parse_framed(raw) else {
            panic!("a complete multibyte response parses");
        };
        assert_eq!(response.body, b"{\"p\":\"\xd0\xbf\"}".as_slice());
    }

    #[test]
    fn parses_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\n{\"ok\r\n7\r\n\":true}\r\n0\r\n\r\n";
        let Parsed::Done {
            response,
            consumed,
            reusable,
        } = parse_framed(raw)
        else {
            panic!("a complete chunked response parses");
        };
        assert_eq!(response.body, b"{\"ok\":true}".as_slice());
        assert_eq!(consumed, raw.len(), "the zero chunk and terminator consume");
        assert!(reusable);
    }

    #[test]
    fn framed_reads_leave_surplus_for_the_next_response() {
        // Two responses in one buffer: the first exchange must consume
        // exactly its own bytes (pipelining guard).
        let first = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
        let second = b"HTTP/1.1 201 Created\r\ncontent-length: 4\r\n\r\nmade";
        let mut raw = first.to_vec();
        raw.extend_from_slice(second);
        let Parsed::Done {
            response, consumed, ..
        } = parse_framed(&raw)
        else {
            panic!("the first response parses");
        };
        assert_eq!(response.status, 200);
        assert_eq!(consumed, first.len());
        let Parsed::Done {
            response: second_response,
            ..
        } = parse_framed(&raw[consumed..])
        else {
            panic!("the second response parses from the surplus");
        };
        assert_eq!(second_response.status, 201);
    }

    #[test]
    fn incomplete_responses_need_more() {
        let truncated_length = b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort";
        assert!(matches!(parse_framed(truncated_length), Parsed::NeedMore));
        let truncated_head = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n";
        assert!(matches!(parse_framed(truncated_head), Parsed::NeedMore));
        let truncated_chunk = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhel";
        assert!(matches!(parse_framed(truncated_chunk), Parsed::NeedMore));
        let head_only_chunk =
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n";
        assert!(matches!(parse_framed(head_only_chunk), Parsed::NeedMore));
    }

    #[test]
    fn close_responses_are_not_reusable() {
        let closed = b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\nok";
        let Parsed::Done { reusable, .. } = parse_framed(closed) else {
            panic!("parses");
        };
        assert!(!reusable);
        let http10 = b"HTTP/1.0 200 OK\r\ncontent-length: 2\r\n\r\nok";
        let Parsed::Done { reusable, .. } = parse_framed(http10) else {
            panic!("parses");
        };
        assert!(!reusable);
    }

    #[test]
    fn oversized_content_length_is_rejected() {
        let raw = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        );
        let Parsed::Invalid(error) = parse_framed(raw.as_bytes()) else {
            panic!("an oversized content-length must fail before the body arrives");
        };
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn oversized_head_is_rejected() {
        // state-audit A.8: MAX_HEAD_BYTES is enforced on the complete
        // head itself, not only as slack on the combined buffer bound.
        let raw = format!(
            "HTTP/1.1 200 OK\r\n{}content-length: 2\r\n\r\nok",
            "x-filler: 8\r\n".repeat(MAX_HEAD_BYTES / 12 + 1)
        );
        let Parsed::Invalid(error) = parse_framed(raw.as_bytes()) else {
            panic!("a head over the 64 KiB cap must fail");
        };
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(error.to_string().contains("64 KiB cap"));
    }

    #[test]
    fn decode_chunked_rejects_an_overflowing_chunk_size() {
        // A server-controlled `FFFFFFFFFFFFFFFF` chunk line must be
        // rejected, not wrap the arithmetic into a panicking slice.
        assert!(matches!(
            decode_chunked_prefix(b"FFFFFFFFFFFFFFFF\r\nx"),
            ChunkedStep::Invalid(_)
        ));
        let ChunkedStep::Done { body, consumed } =
            decode_chunked_prefix(b"5\r\nhello\r\n0\r\n\r\n")
        else {
            panic!("a plain chunked body decodes");
        };
        assert_eq!(body, b"hello".as_slice());
        assert_eq!(consumed, b"5\r\nhello\r\n0\r\n\r\n".len());
    }

    #[test]
    fn chunk_sizes_are_hex_not_decimal() {
        // Real-server case (live probe caught this): nginx writes chunk
        // sizes in hex — `a` is ten bytes, and a decimal parse both
        // misframes `10`-style sizes and rejects `ff` outright.
        let mut raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n".to_vec();
        raw.extend_from_slice(b"ff\r\n"); // hex 255
        raw.extend_from_slice("0123456789".repeat(25).as_bytes()); // 250
        raw.extend_from_slice(b"01234\r\n"); // 255 total
        raw.extend_from_slice(b"a\r\n0123456789\r\n"); // hex a = 10
        raw.extend_from_slice(b"0\r\n\r\n");
        let Parsed::Done {
            response, consumed, ..
        } = parse_framed(&raw)
        else {
            panic!("hex chunk sizes parse");
        };
        assert_eq!(response.body.len(), 255 + 10);
        assert_eq!(consumed, raw.len());
    }

    #[test]
    fn chunked_trailers_consume_through_the_terminator() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\nx-trailer: y\r\n\r\nsurplus";
        let Parsed::Done {
            response, consumed, ..
        } = parse_framed(raw)
        else {
            panic!("a trailed chunked response parses");
        };
        assert_eq!(response.body, b"hi".as_slice());
        assert_eq!(&raw[consumed..], b"surplus");
    }

    // ── request head ──

    #[test]
    fn request_head_refuses_control_characters() {
        let head = request_head(
            "GET",
            "/api/v1/channels.history",
            "host",
            "application/json",
            &[],
            0,
        )
        .unwrap();
        assert!(head.contains("connection: keep-alive"));
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

    // ── live socket behavior ──

    /// Reads one request head (plus its content-length body) off a test
    /// server connection and returns the head text.
    async fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
        let mut buf: Vec<u8> = Vec::new();
        let head_len = loop {
            if let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break split + 4;
            }
            let BufResult(read, filled) = stream.read(Vec::with_capacity(4096)).await;
            read?;
            if filled.is_empty() {
                return Err(std::io::Error::other("test client disconnected"));
            }
            buf.extend_from_slice(&filled);
        };
        let length = String::from_utf8_lossy(&buf[..head_len])
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buf.len() < head_len + length {
            let BufResult(read, filled) = stream.read(Vec::with_capacity(4096)).await;
            read?;
            if filled.is_empty() {
                break;
            }
            buf.extend_from_slice(&filled);
        }
        Ok(String::from_utf8_lossy(&buf[..head_len]).into_owned())
    }

    /// Serves planned connections: `scripts[i]` lists the responses for
    /// the i-th accepted connection, in order; each connection closes
    /// after its script ends. `accepts` counts connections, `closed`
    /// flips after every connection close — the sync point that makes
    /// stale-reuse tests deterministic.
    async fn serve_planned(
        listener: compio::net::TcpListener,
        scripts: Vec<Vec<String>>,
        accepts: Arc<AtomicUsize>,
        closed: Arc<AtomicBool>,
    ) -> std::io::Result<()> {
        for script in scripts {
            let (mut stream, _) = listener.accept().await?;
            accepts.fetch_add(1, Ordering::SeqCst);
            for response in script {
                read_request(&mut stream).await?;
                let BufResult(written, _) = stream.write_all(response.into_bytes()).await;
                written?;
            }
            drop(stream);
            closed.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn close_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// HTTP/1.0-style: no framing, the body ends when the connection
    /// closes.
    fn eof_response(body: &str) -> String {
        format!("HTTP/1.0 200 OK\r\ncontent-type: application/json\r\n\r\n{body}")
    }

    fn get(path: &'static str) -> Request<'static> {
        Request {
            method: "GET",
            path,
            headers: Vec::new(),
            body: None,
        }
    }

    fn transport_to(addr: &std::net::SocketAddr) -> HttpTransport {
        HttpTransport::new(
            (addr.ip().to_string(), addr.port()),
            addr.ip().to_string(),
            false,
        )
    }

    /// Spins until the server confirms a connection close (bounded).
    async fn wait_closed(closed: &AtomicBool) {
        for _ in 0..100 {
            if closed.load(Ordering::SeqCst) {
                return;
            }
            compio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the server never confirmed the connection close");
    }

    #[test]
    fn keep_alive_two_calls_share_one_connection() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let accepts = Arc::new(AtomicUsize::new(0));
            let closed = Arc::new(AtomicBool::new(false));
            compio::runtime::spawn(serve_planned(
                listener,
                vec![vec![
                    json_response("{\"call\":1}"),
                    json_response("{\"call\":2}"),
                ]],
                accepts.clone(),
                closed.clone(),
            ))
            .detach();
            let transport = transport_to(&addr);
            let first = transport.call(get("/api/v1/rooms.get")).await.unwrap();
            assert_eq!(first.body, "{\"call\":1}");
            let second = transport.call(get("/api/v1/rooms.get")).await.unwrap();
            assert_eq!(second.body, "{\"call\":2}");
            assert_eq!(
                accepts.load(Ordering::SeqCst),
                1,
                "both calls ride one connection"
            );
        });
    }

    #[test]
    fn stale_reused_connection_retries_get_once() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let accepts = Arc::new(AtomicUsize::new(0));
            let closed = Arc::new(AtomicBool::new(false));
            compio::runtime::spawn(serve_planned(
                listener,
                vec![
                    vec![json_response("{\"call\":1}")],
                    vec![json_response("{\"call\":2}")],
                ],
                accepts.clone(),
                closed.clone(),
            ))
            .detach();
            let transport = transport_to(&addr);
            let first = transport.call(get("/api/v1/rooms.get")).await.unwrap();
            assert_eq!(first.body, "{\"call\":1}");
            // The server drops the idle connection before the next call:
            // the classic idle-close race.
            wait_closed(&closed).await;
            let second = transport.call(get("/api/v1/rooms.get")).await.unwrap();
            assert_eq!(second.body, "{\"call\":2}", "the retry lands fresh");
            assert_eq!(accepts.load(Ordering::SeqCst), 2, "one fresh reconnect");
        });
    }

    #[test]
    fn stale_reused_connection_does_not_retry_post() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let accepts = Arc::new(AtomicUsize::new(0));
            let closed = Arc::new(AtomicBool::new(false));
            compio::runtime::spawn(serve_planned(
                listener,
                vec![vec![json_response("{\"sent\":true}")]],
                accepts.clone(),
                closed.clone(),
            ))
            .detach();
            let transport = transport_to(&addr);
            let sent = transport.call(get("/api/v1/rooms.get")).await.unwrap();
            assert_eq!(sent.body, "{\"sent\":true}");
            wait_closed(&closed).await;
            let post = transport
                .call(Request {
                    method: "POST",
                    path: "/api/v1/chat.sendMessage",
                    headers: Vec::new(),
                    body: Some(b"{}".to_vec()),
                })
                .await;
            assert!(
                post.is_err(),
                "a POST on a dead connection must fail, not blind-retry"
            );
            assert_eq!(
                accepts.load(Ordering::SeqCst),
                1,
                "no second connection was dialed for the POST"
            );
        });
    }

    #[test]
    fn connection_close_response_forces_fresh_connect() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let accepts = Arc::new(AtomicUsize::new(0));
            let closed = Arc::new(AtomicBool::new(false));
            compio::runtime::spawn(serve_planned(
                listener,
                vec![
                    vec![close_response("{\"call\":1}")],
                    vec![close_response("{\"call\":2}")],
                ],
                accepts.clone(),
                closed.clone(),
            ))
            .detach();
            let transport = transport_to(&addr);
            let first = transport.call(get("/api/v1/rooms.get")).await.unwrap();
            assert_eq!(first.body, "{\"call\":1}");
            let second = transport.call(get("/api/v1/rooms.get")).await.unwrap();
            assert_eq!(second.body, "{\"call\":2}");
            assert_eq!(
                accepts.load(Ordering::SeqCst),
                2,
                "connection: close is honored — every call reconnects"
            );
        });
    }

    #[test]
    fn eof_delimited_response_reads_until_close() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let accepts = Arc::new(AtomicUsize::new(0));
            let closed = Arc::new(AtomicBool::new(false));
            compio::runtime::spawn(serve_planned(
                listener,
                vec![
                    vec![eof_response("{\"eof\":true}")],
                    vec![json_response("{\"call\":2}")],
                ],
                accepts.clone(),
                closed.clone(),
            ))
            .detach();
            let transport = transport_to(&addr);
            let first = transport.call(get("/api/v1/rooms.get")).await.unwrap();
            assert_eq!(first.body, "{\"eof\":true}");
            let second = transport.call(get("/api/v1/rooms.get")).await.unwrap();
            assert_eq!(second.body, "{\"call\":2}");
            assert_eq!(
                accepts.load(Ordering::SeqCst),
                2,
                "an EOF-delimited response never pools its connection"
            );
        });
    }

    #[test]
    fn truncated_framed_body_is_an_error() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let accepts = Arc::new(AtomicUsize::new(0));
            let closed = Arc::new(AtomicBool::new(false));
            compio::runtime::spawn(serve_planned(
                listener,
                vec![vec![
                    "HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort".to_owned(),
                ]],
                accepts.clone(),
                closed.clone(),
            ))
            .detach();
            let transport = transport_to(&addr);
            let result = transport.call(get("/api/v1/rooms.get")).await;
            assert!(result.is_err(), "a body cut at EOF must fail, not truncate");
            assert_eq!(accepts.load(Ordering::SeqCst), 1);
        });
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
    fn response_at_close_enforces_the_head_cap() {
        // state-audit r2 SF-1: the EOF path applies the same 64 KiB
        // head bound as the keep-alive path.
        let mut buf = format!(
            "HTTP/1.1 200 OK\r\n{}content-length: 4\r\n\r\nbody",
            "x-filler: 8\r\n".repeat(MAX_HEAD_BYTES / 12 + 1)
        )
        .into_bytes();
        let Err(error) = response_at_close(&mut buf) else {
            panic!("a close-path head over the 64 KiB cap must fail");
        };
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        // An ordinary until-close response still resolves.
        let mut buf =
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"eof\":true}".to_vec();
        let response = response_at_close(&mut buf).expect("a small EOF response resolves");
        assert_eq!(response.body, b"{\"eof\":true}".as_slice());
        assert!(buf.is_empty(), "the consumed buffer is drained");
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
