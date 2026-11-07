//! Minimal HTTP/1.1 client on compio sockets — the transport layer under the
//! Rocket.Chat protocol client: request line, headers, one content-length or
//! chunked body, connection closed per request. [`Transport`] is the seam the
//! protocol layer depends on; [`HttpTransport`] is the compio implementation,
//! and protocol tests substitute their own. TLS joins in the slice that
//! targets real (https) servers; the dev server is plain HTTP on localhost.

use std::io::ErrorKind;

use compio::buf::BufResult;
use compio::io::{AsyncReadExt, AsyncWriteExt};
use compio::net::TcpStream;

/// One outbound request; `host` goes into the Host header.
pub struct Request<'a> {
    pub method: &'a str,
    /// Path including the `/api/v1` prefix, e.g. `/api/v1/login`.
    pub path: &'a str,
    pub host: &'a str,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
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

/// The compio implementation: one TCP connection per call.
pub struct HttpTransport {
    addr: (String, u16),
}

impl HttpTransport {
    pub fn new(addr: (String, u16)) -> Self {
        Self { addr }
    }
}

impl Transport for HttpTransport {
    async fn call(&self, req: Request<'_>) -> std::io::Result<Response> {
        let mut stream = TcpStream::connect(self.addr.clone()).await?;

        let mut head = format!(
            "{} {} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\ncontent-type: application/json\r\n",
            req.method, req.path, req.host
        );
        for (name, value) in &req.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        let body = req.body.unwrap_or_default();
        head.push_str(&format!("content-length: {}\r\n\r\n", body.len()));

        let BufResult(written, _) = stream.write_all(head.into_bytes()).await;
        written?;
        let BufResult(wrote_body, _) = stream.write_all(body.into_bytes()).await;
        wrote_body?;

        let BufResult(_, raw) = stream.read_to_end(Vec::with_capacity(16 * 1024)).await;
        parse_response(&String::from_utf8_lossy(&raw))
    }
}

/// Splits head/body, decodes content-length or chunked bodies, and reports
/// the status code. Truncated reads surface as InvalidData, not empty bodies.
fn parse_response(raw: &str) -> std::io::Result<Response> {
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| invalid("no HTTP header/body separator"))?;

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
        body[..length].to_owned()
    } else {
        body.to_owned()
    };

    Ok(Response { status, body })
}

/// Decodes `length\r\n<bytes>\r\n` chunks until the zero chunk.
fn decode_chunked(body: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let (size_line, next) = rest.split_once("\r\n")?;
        let size = usize::from_str_radix(size_line.split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            return Some(out);
        }
        if next.len() < size {
            return None;
        }
        out.push_str(&next[..size]);
        rest = &next[size..];
        rest = rest.strip_prefix("\r\n").unwrap_or(rest);
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
        let raw = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"ok\":true}");
    }

    #[test]
    fn parses_chunked_body() {
        let raw = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\n{\"ok\r\n7\r\n\":true}\r\n0\r\n\r\n";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"ok\":true}");
    }

    #[test]
    fn rejects_truncated_body() {
        let raw = "HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort";
        assert!(parse_response(raw).is_err());
    }
}
