//! NDJSON framing: one JSON value per line, `\n` terminated, `\r\n` tolerated.
//!
//! Incoming lines are capped at [`MAX_READ_LINE`] (4 MiB): a pool that sends more without a
//! newline is treated as broken and the session is dropped (no attempt to resynchronise).
//! Outgoing lines are guarded at [`MAX_WRITE_LINE`] (2 MiB): our largest message is a proof
//! submit (100–370 KB of base64), so anything bigger is a bug and is never put on the wire.
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Incoming line cap, excluding the terminator.
pub const MAX_READ_LINE: usize = crate::MAX_LINE_BYTES;
/// Outgoing line guard, including the terminator.
pub const MAX_WRITE_LINE: usize = 2 * 1024 * 1024;

const READ_CHUNK: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("incoming line exceeds {limit} bytes")]
    ReadTooLong { limit: usize },
    #[error("outgoing line of {len} bytes exceeds the {limit}-byte write guard")]
    WriteTooLong { len: usize, limit: usize },
    #[error("incoming line is not UTF-8")]
    NotUtf8,
    #[error("outgoing line contains a raw newline")]
    EmbeddedNewline,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Incremental line splitter over a byte stream; usable from sync or async code.
#[derive(Debug)]
pub struct LineBuffer {
    buf: Vec<u8>,
    /// Start of the unconsumed data in `buf`.
    start: usize,
    /// Bytes after `start` already searched for `\n`.
    scanned: usize,
    cap: usize,
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new(MAX_READ_LINE)
    }
}

impl LineBuffer {
    pub fn new(cap: usize) -> Self {
        LineBuffer { buf: Vec::new(), start: 0, scanned: 0, cap }
    }

    /// Append raw bytes from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start >= self.buf.len() / 2 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Unconsumed bytes (a partial line).
    pub fn pending(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Next complete, non-blank line without its terminator. Blank lines are skipped.
    pub fn next_line(&mut self) -> Result<Option<String>, CodecError> {
        loop {
            let data = &self.buf[self.start..];
            let Some(pos) = data[self.scanned..].iter().position(|&b| b == b'\n') else {
                self.scanned = data.len();
                // A full-size line may still be waiting for the `\n` of its `\r\n`.
                let partial = data.strip_suffix(b"\r".as_slice()).unwrap_or(data);
                if partial.len() > self.cap {
                    return Err(CodecError::ReadTooLong { limit: self.cap });
                }
                return Ok(None);
            };
            let end = self.scanned + pos;
            let mut line = &data[..end];
            if let [head @ .., b'\r'] = line {
                line = head;
            }
            if line.len() > self.cap {
                return Err(CodecError::ReadTooLong { limit: self.cap });
            }
            let text = std::str::from_utf8(line).map_err(|_| CodecError::NotUtf8)?;
            let out = (!text.trim().is_empty()).then(|| text.to_string());
            self.start += end + 1;
            self.scanned = 0;
            if out.is_some() {
                return Ok(out);
            }
        }
    }

    /// At end of stream: the unterminated tail, if it is not blank (tolerant of a missing final `\n`).
    pub fn take_tail(&mut self) -> Result<Option<String>, CodecError> {
        let data = &self.buf[self.start..];
        let data = data.strip_suffix(b"\r".as_slice()).unwrap_or(data);
        if data.len() > self.cap {
            return Err(CodecError::ReadTooLong { limit: self.cap });
        }
        let text = std::str::from_utf8(data).map_err(|_| CodecError::NotUtf8)?;
        let out = (!text.trim().is_empty()).then(|| text.to_string());
        self.buf.clear();
        self.start = 0;
        self.scanned = 0;
        Ok(out)
    }
}

/// Async NDJSON reader. `next_line` is cancel-safe (the only await point is the socket read,
/// and bytes are buffered before any other work), so it can sit in a `tokio::select!`.
#[derive(Debug)]
pub struct NdjsonReader<R> {
    inner: R,
    buf: LineBuffer,
    chunk: Box<[u8]>,
    eof: bool,
}

impl<R: AsyncRead + Unpin> NdjsonReader<R> {
    pub fn new(inner: R) -> Self {
        Self::with_cap(inner, MAX_READ_LINE)
    }

    pub fn with_cap(inner: R, cap: usize) -> Self {
        NdjsonReader { inner, buf: LineBuffer::new(cap), chunk: vec![0u8; READ_CHUNK].into_boxed_slice(), eof: false }
    }

    /// Next non-blank line; `Ok(None)` at end of stream.
    pub async fn next_line(&mut self) -> Result<Option<String>, CodecError> {
        loop {
            if let Some(line) = self.buf.next_line()? {
                return Ok(Some(line));
            }
            if self.eof {
                return self.buf.take_tail();
            }
            let n = self.inner.read(&mut self.chunk).await?;
            if n == 0 {
                self.eof = true;
            } else {
                self.buf.push(&self.chunk[..n]);
            }
        }
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

/// Serialize `msg` compactly and append `\n`, enforcing the write guard.
pub fn encode_line<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    let mut v = serde_json::to_vec(msg)?;
    v.push(b'\n');
    if v.len() > MAX_WRITE_LINE {
        return Err(CodecError::WriteTooLong { len: v.len(), limit: MAX_WRITE_LINE });
    }
    Ok(v)
}

/// Frame an already serialized line, enforcing the write guard and single-line shape.
pub fn frame_line(line: &str) -> Result<Vec<u8>, CodecError> {
    if line.bytes().any(|b| b == b'\n' || b == b'\r') {
        return Err(CodecError::EmbeddedNewline);
    }
    let len = line.len() + 1;
    if len > MAX_WRITE_LINE {
        return Err(CodecError::WriteTooLong { len, limit: MAX_WRITE_LINE });
    }
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(line.as_bytes());
    v.push(b'\n');
    Ok(v)
}

/// Write one framed line and flush.
pub async fn write_line<W: AsyncWrite + Unpin>(w: &mut W, line: &str) -> Result<(), CodecError> {
    let framed = frame_line(line)?;
    w.write_all(&framed).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lf_and_crlf_and_skips_blank_lines() {
        let mut b = LineBuffer::new(1024);
        b.push(b"{\"a\":1}\r\n\r\n\n{\"b\"");
        assert_eq!(b.next_line().unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(b.next_line().unwrap(), None);
        b.push(b":2}\n");
        assert_eq!(b.next_line().unwrap().as_deref(), Some("{\"b\":2}"));
        assert_eq!(b.next_line().unwrap(), None);
        assert_eq!(b.pending(), 0);
    }

    #[test]
    fn byte_at_a_time_feed() {
        let text = b"one\r\ntwo\nthree\n";
        let mut b = LineBuffer::new(1024);
        let mut got = vec![];
        for &c in text.iter() {
            b.push(&[c]);
            while let Some(l) = b.next_line().unwrap() {
                got.push(l);
            }
        }
        assert_eq!(got, ["one", "two", "three"]);
    }

    #[test]
    fn read_cap_is_enforced_with_and_without_newline() {
        let mut b = LineBuffer::new(8);
        b.push(b"12345678\n");
        assert_eq!(b.next_line().unwrap().as_deref(), Some("12345678"));
        b.push(b"123456789\n");
        assert!(matches!(b.next_line(), Err(CodecError::ReadTooLong { limit: 8 })));
        let mut b = LineBuffer::new(8);
        b.push(b"123456789");
        assert!(matches!(b.next_line(), Err(CodecError::ReadTooLong { limit: 8 })));
        // CRLF does not count towards the cap, even when the `\n` arrives separately.
        let mut b = LineBuffer::new(8);
        b.push(b"12345678\r\n");
        assert_eq!(b.next_line().unwrap().as_deref(), Some("12345678"));
        let mut b = LineBuffer::new(8);
        b.push(b"12345678\r");
        assert_eq!(b.next_line().unwrap(), None);
        b.push(b"\n");
        assert_eq!(b.next_line().unwrap().as_deref(), Some("12345678"));
        let mut b = LineBuffer::new(8);
        b.push(b"123456789\r");
        assert!(matches!(b.next_line(), Err(CodecError::ReadTooLong { limit: 8 })));
    }

    #[test]
    fn invalid_utf8_is_an_error() {
        let mut b = LineBuffer::new(64);
        b.push(&[0xff, 0xfe, b'\n']);
        assert!(matches!(b.next_line(), Err(CodecError::NotUtf8)));
    }

    #[test]
    fn write_guard_and_embedded_newline() {
        assert_eq!(frame_line("{}").unwrap(), b"{}\n");
        assert!(matches!(frame_line("{}\n{}"), Err(CodecError::EmbeddedNewline)));
        let big = "x".repeat(MAX_WRITE_LINE);
        assert!(matches!(frame_line(&big), Err(CodecError::WriteTooLong { .. })));
        let ok = "x".repeat(MAX_WRITE_LINE - 1);
        assert_eq!(frame_line(&ok).unwrap().len(), MAX_WRITE_LINE);
        let v = serde_json::json!({"s": "a\nb"});
        assert_eq!(encode_line(&v).unwrap(), b"{\"s\":\"a\\nb\"}\n");
    }

    #[tokio::test]
    async fn async_reader_handles_tail_and_eof() {
        let data: &[u8] = b"{\"x\":1}\r\n\n{\"y\":2}";
        let mut r = NdjsonReader::new(data);
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("{\"x\":1}"));
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("{\"y\":2}"));
        assert_eq!(r.next_line().await.unwrap(), None);
        assert_eq!(r.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn async_reader_rejects_a_4mib_line() {
        let mut data = vec![b' '; MAX_READ_LINE + 1];
        data.push(b'\n');
        let mut r = NdjsonReader::new(&data[..]);
        assert!(matches!(r.next_line().await, Err(CodecError::ReadTooLong { .. })));
    }
}
