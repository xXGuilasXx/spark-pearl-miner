//! A minimal HTTP/1.1 client for one job: `GET` a local metrics page. Plain `http://` only,
//! `Connection: close`, `Content-Length`, chunked or read-to-close bodies, a size cap and one
//! overall timeout. Anything fancier (TLS, redirects, keep-alive) is deliberately missing.

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::prom::{parse_vllm_load, PromError, VllmLoad};

/// Largest response accepted (vLLM's metrics are ~60 KB).
pub const MAX_RESPONSE_BYTES: usize = 4 << 20;
/// Largest header block accepted.
pub const MAX_HEADER_BYTES: usize = 64 << 10;

/// A parsed `http://host[:port]/path` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpUrl {
    /// Host name or IP; IPv6 literals without brackets.
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
}

impl HttpUrl {
    /// Parses a plain-HTTP URL (default port 80, default path `/`).
    pub fn parse(url: &str) -> Result<Self, HttpError> {
        let bad = || HttpError::BadUrl(url.to_string());
        let rest = url.trim().strip_prefix("http://").ok_or_else(bad)?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() || authority.contains('@') {
            return Err(bad());
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let end = v6.find(']').ok_or_else(bad)?;
            let port = match &v6[end + 1..] {
                "" => 80,
                p => p.strip_prefix(':').and_then(|p| p.parse().ok()).ok_or_else(bad)?,
            };
            (&v6[..end], port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, p.parse().map_err(|_| bad())?),
                None => (authority, 80),
            }
        };
        if host.is_empty() || path.contains(char::is_whitespace) {
            return Err(bad());
        }
        Ok(HttpUrl { host: host.to_string(), port, path: path.to_string() })
    }

    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Why a fetch failed.
#[derive(Debug)]
pub enum HttpError {
    BadUrl(String),
    Timeout,
    Io(std::io::Error),
    TooLarge,
    Malformed(&'static str),
    /// A non-2xx status.
    Status(u16),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::BadUrl(u) => write!(f, "not a plain http:// URL: {u:?}"),
            HttpError::Timeout => f.write_str("timed out"),
            HttpError::Io(e) => write!(f, "{e}"),
            HttpError::TooLarge => write!(f, "response larger than {MAX_RESPONSE_BYTES} bytes"),
            HttpError::Malformed(m) => write!(f, "malformed HTTP response: {m}"),
            HttpError::Status(s) => write!(f, "HTTP status {s}"),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        HttpError::Io(e)
    }
}

/// A complete response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Parses `raw` as far as possible. `Ok(None)`: more bytes are needed (only when `eof` is
/// false).
pub fn parse_response(raw: &[u8], eof: bool) -> Result<Option<Response>, HttpError> {
    let Some(head_end) = find(raw, b"\r\n\r\n") else {
        if raw.len() > MAX_HEADER_BYTES {
            return Err(HttpError::Malformed("header block too large"));
        }
        return if eof { Err(HttpError::Malformed("truncated headers")) } else { Ok(None) };
    };
    let head = std::str::from_utf8(&raw[..head_end])
        .map_err(|_| HttpError::Malformed("non-UTF-8 headers"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::Malformed("not an HTTP/1.x status line"));
    }
    let status: u16 =
        parts.next().and_then(|s| s.parse().ok()).ok_or(HttpError::Malformed("bad status code"))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::Malformed("bad header line"));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let n = value.parse().map_err(|_| HttpError::Malformed("bad content-length"))?;
            content_length = Some(n);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }

    let body = &raw[head_end + 4..];
    let complete = if chunked {
        match decode_chunked(body).map_err(HttpError::Malformed)? {
            Some(b) => Some(b),
            None if eof => return Err(HttpError::Malformed("truncated chunked body")),
            None => None,
        }
    } else if let Some(n) = content_length {
        if n > MAX_RESPONSE_BYTES {
            return Err(HttpError::TooLarge);
        }
        if body.len() >= n {
            Some(body[..n].to_vec())
        } else if eof {
            return Err(HttpError::Malformed("truncated body"));
        } else {
            None
        }
    } else if eof {
        Some(body.to_vec())
    } else {
        None
    };
    Ok(complete.map(|body| Response { status, body }))
}

/// Decodes a chunked body. `Ok(None)` when incomplete.
fn decode_chunked(mut buf: &[u8]) -> Result<Option<Vec<u8>>, &'static str> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = find(buf, b"\r\n") else { return Ok(None) };
        let size_line = std::str::from_utf8(&buf[..eol]).map_err(|_| "bad chunk size")?;
        let size_hex = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| "bad chunk size")?;
        buf = &buf[eol + 2..];
        if size == 0 {
            // Optional trailers, then an empty line.
            loop {
                let Some(eol) = find(buf, b"\r\n") else { return Ok(None) };
                if eol == 0 {
                    return Ok(Some(out));
                }
                buf = &buf[eol + 2..];
            }
        }
        if out.len() + size > MAX_RESPONSE_BYTES {
            return Err("chunked body too large");
        }
        if buf.len() < size + 2 {
            return Ok(None);
        }
        if &buf[size..size + 2] != b"\r\n" {
            return Err("chunk not terminated by CRLF");
        }
        out.extend_from_slice(&buf[..size]);
        buf = &buf[size + 2..];
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `GET url`, returning the body of a 2xx response. The whole exchange must finish within
/// `timeout`.
pub async fn get(url: &HttpUrl, timeout: Duration) -> Result<Vec<u8>, HttpError> {
    match tokio::time::timeout(timeout, get_inner(url)).await {
        Ok(r) => r,
        Err(_) => Err(HttpError::Timeout),
    }
}

async fn get_inner(url: &HttpUrl) -> Result<Vec<u8>, HttpError> {
    let mut stream = TcpStream::connect((url.host.as_str(), url.port)).await?;
    stream.set_nodelay(true)?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: spark-pearl-miner\r\nAccept: text/plain\r\n\
         Connection: close\r\n\r\n",
        url.path,
        url.host_header()
    );
    stream.write_all(request.as_bytes()).await?;

    let mut raw = Vec::with_capacity(64 << 10);
    let mut buf = vec![0u8; 16 << 10];
    loop {
        let n = stream.read(&mut buf).await?;
        let eof = n == 0;
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > MAX_RESPONSE_BYTES + MAX_HEADER_BYTES {
            return Err(HttpError::TooLarge);
        }
        if let Some(resp) = parse_response(&raw, eof)? {
            if !(200..300).contains(&resp.status) {
                return Err(HttpError::Status(resp.status));
            }
            return Ok(resp.body);
        }
    }
}

/// Why the vLLM load could not be read.
#[derive(Debug)]
pub enum FetchError {
    Http(HttpError),
    Metrics(PromError),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Http(e) => write!(f, "vLLM metrics: {e}"),
            FetchError::Metrics(e) => write!(f, "vLLM metrics: {e}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Fetches `url` and reads the two vLLM gauges.
pub async fn fetch_vllm_load(url: &HttpUrl, timeout: Duration) -> Result<VllmLoad, FetchError> {
    let body = get(url, timeout).await.map_err(FetchError::Http)?;
    parse_vllm_load(&String::from_utf8_lossy(&body)).map_err(FetchError::Metrics)
}
