//! Minimal HTTP/1.1 framing for the remote helper transport (`F-13` D-113
//! path (b)).
//!
//! The wire contract inside the HTTP exchange stays byte-identical to the Unix
//! socket transport: the client sends one long-lived `POST /rpc` whose request
//! body carries the NDJSON client frames and whose response body carries the
//! NDJSON server frames, both `Transfer-Encoding: chunked`. [`HttpTunnel`]
//! exposes that pair of chunked bodies as a single duplex byte stream, so the
//! existing NDJSON framing, multiplexing, request cancellation and
//! `PROTOCOL_VERSION` handshake run unchanged.
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, BufReader, ReadBuf};

/// Maximum accepted HTTP head (request or response), including CRLFs.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_CHUNK_LINE_BYTES: usize = 1024;
const MAX_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PENDING_WRITE_BYTES: usize = 1024 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// A parsed HTTP head (request or response).
#[derive(Debug, Clone)]
pub struct HttpHead {
    pub first_line: String,
    headers: Vec<(String, String)>,
}

impl HttpHead {
    /// Case-insensitive header lookup (first match).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

/// Read one HTTP head from `reader` up to [`MAX_HEAD_BYTES`].
///
/// Works on a [`BufReader`] so any bytes past the head stay buffered for the
/// tunnel built over the same reader.
pub async fn read_head<S: AsyncRead + Unpin>(
    reader: &mut BufReader<S>,
    max: usize,
) -> io::Result<HttpHead> {
    let mut raw = Vec::with_capacity(256);
    loop {
        if raw.len() >= max {
            return Err(invalid_data("HTTP head exceeds its size limit"));
        }
        let byte = reader.read_u8().await?;
        raw.push(byte);
        if raw.ends_with(b"\r\n\r\n") {
            return parse_head(&raw);
        }
    }
}

/// Parse a complete head (terminated by a blank line).
pub fn parse_head(raw: &[u8]) -> io::Result<HttpHead> {
    let text = std::str::from_utf8(raw).map_err(|_| invalid_data("HTTP head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let first_line = lines
        .next()
        .filter(|line| !line.is_empty() && line.len() <= 1024)
        .ok_or_else(|| invalid_data("HTTP head has no start line"))?
        .to_owned();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid_data("HTTP header line is malformed"))?;
        let name = name.trim();
        if name.is_empty()
            || name.len() > 256
            || name.bytes().any(|b| b.is_ascii_control() || b == b' ')
        {
            return Err(invalid_data("HTTP header name is invalid"));
        }
        let value = value.trim();
        if value.len() > 8192 || value.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n') {
            return Err(invalid_data("HTTP header value is invalid"));
        }
        headers.push((name.to_owned(), value.to_owned()));
        if headers.len() > 128 {
            return Err(invalid_data("HTTP head carries too many headers"));
        }
    }
    Ok(HttpHead {
        first_line: first_line.to_owned(),
        headers,
    })
}

/// First line of a request head split into `(method, target)`.
pub fn parse_request_line(head: &HttpHead) -> io::Result<(&str, &str)> {
    let mut parts = head.first_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid_data("HTTP request line is malformed"));
    };
    if method.is_empty()
        || target.is_empty()
        || target.len() > 2048
        || !version.starts_with("HTTP/1.")
    {
        return Err(invalid_data("HTTP request line is malformed"));
    }
    Ok((method, target))
}

/// Status code of a response head.
pub fn parse_status_line(head: &HttpHead) -> io::Result<u16> {
    let mut parts = head.first_line.split(' ');
    let (Some(version), Some(code)) = (parts.next(), parts.next()) else {
        return Err(invalid_data("HTTP status line is malformed"));
    };
    if !version.starts_with("HTTP/1.") || code.len() != 3 {
        return Err(invalid_data("HTTP status line is malformed"));
    }
    code.parse::<u16>()
        .map_err(|_| invalid_data("HTTP status line is malformed"))
}

/// Write a fixed response head (no body). Used for auth/protocol rejections.
pub async fn write_error_response<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: &str,
    extra_headers: &[(&str, &str)],
) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\nContent-Type: text/plain; charset=utf-8\r\n"
    );
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await
}

/// Write the 200 response head that opens the NDJSON tunnel.
pub async fn write_tunnel_response<S: AsyncWrite + Unpin>(stream: &mut S) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nX-Soyo-Protocol: {}\r\n\r\n",
        crate::protocol::PROTOCOL_VERSION
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await
}

/// Write the tunnel-opening request head.
pub async fn write_tunnel_request<S: AsyncWrite + Unpin>(
    stream: &mut S,
    authority: &str,
    token: &str,
) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let head = format!(
        "POST /rpc HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\
         Connection: close\r\nX-Soyo-Protocol: {}\r\n\r\n",
        crate::protocol::PROTOCOL_VERSION
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadState {
    Size,
    Data,
    DataCrlf,
    Trailers,
    Done,
}

/// Duplex chunked-body adapter.
///
/// Reads are de-chunked from `inner`; writes are chunk-encoded onto `inner`.
/// A zero-size chunk ends the read side; shutdown writes the terminating chunk.
pub struct HttpTunnel<S> {
    inner: S,
    read_buf: Vec<u8>,
    read_pos: usize,
    state: ReadState,
    remaining: u64,
    pending: Vec<u8>,
    pending_pos: usize,
    terminator_queued: bool,
}

impl<S> HttpTunnel<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            read_buf: Vec::new(),
            read_pos: 0,
            state: ReadState::Size,
            remaining: 0,
            pending: Vec::new(),
            pending_pos: 0,
            terminator_queued: false,
        }
    }

    fn compact(&mut self) {
        if self.read_pos == self.read_buf.len() {
            self.read_buf.clear();
            self.read_pos = 0;
        }
    }

    fn buffered(&self) -> usize {
        self.read_buf.len() - self.read_pos
    }

    /// Pull more bytes from the inner stream.
    fn fill(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>
    where
        S: AsyncRead + Unpin,
    {
        let mut buffer = [0u8; READ_CHUNK_BYTES];
        let mut read_buf = ReadBuf::new(&mut buffer);
        ready!(Pin::new(&mut self.inner).poll_read(cx, &mut read_buf))?;
        if read_buf.filled().is_empty() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "chunked body ended before its terminating chunk",
            )));
        }
        self.read_buf.extend_from_slice(read_buf.filled());
        Poll::Ready(Ok(()))
    }

    fn try_take_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        if let Some(offset) = self.read_buf[self.read_pos..]
            .iter()
            .position(|&byte| byte == b'\n')
        {
            let end = self.read_pos + offset;
            let mut line = self.read_buf[self.read_pos..end].to_vec();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.read_pos = end + 1;
            self.compact();
            return Ok(Some(line));
        }
        if self.buffered() > MAX_CHUNK_LINE_BYTES {
            return Err(invalid_data("chunk header line exceeds its size limit"));
        }
        Ok(None)
    }

    fn pending_len(&self) -> usize {
        self.pending.len() - self.pending_pos
    }

    /// Push buffered chunk-encoded bytes to the inner stream. `Pending` means
    /// the peer applied backpressure; buffered bytes are retained.
    fn flush_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>
    where
        S: AsyncWrite + Unpin,
    {
        loop {
            if self.pending_pos >= self.pending.len() {
                self.pending.clear();
                self.pending_pos = 0;
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.pending_pos..]) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "chunked body write made no progress",
                    )));
                }
                Poll::Ready(Ok(written)) => self.pending_pos += written,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn begin_data_crlf(&mut self) -> io::Result<()>
    where
        S: AsyncRead + Unpin,
    {
        match self.try_take_line()? {
            Some(line) if line.is_empty() => {
                self.state = ReadState::Size;
                Ok(())
            }
            Some(_) => Err(invalid_data("chunk data is not followed by CRLF")),
            None => Ok(()), // need more bytes; state stays DataCrlf
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for HttpTunnel<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            match self.state {
                ReadState::Done => return Poll::Ready(Ok(())),
                ReadState::Data => {
                    if self.remaining == 0 {
                        self.state = ReadState::DataCrlf;
                        continue;
                    }
                    if self.buffered() > 0 {
                        let available = self.buffered();
                        let take = available
                            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX))
                            .min(buf.remaining());
                        buf.put_slice(&self.read_buf[self.read_pos..self.read_pos + take]);
                        self.read_pos += take;
                        self.remaining -= take as u64;
                        self.compact();
                        return Poll::Ready(Ok(()));
                    }
                    ready!(self.as_mut().get_mut().fill(cx))?;
                }
                ReadState::DataCrlf => {
                    let before = self.state;
                    self.begin_data_crlf()?;
                    if self.state == before {
                        ready!(self.as_mut().get_mut().fill(cx))?;
                    }
                }
                ReadState::Size => match self.try_take_line()? {
                    Some(line) => {
                        let size_part = line.split(|&byte| byte == b';').next().unwrap_or(&line);
                        let text = std::str::from_utf8(size_part)
                            .map_err(|_| invalid_data("chunk size is not UTF-8"))?
                            .trim();
                        if text.is_empty() || text.len() > 16 {
                            return Poll::Ready(Err(invalid_data("chunk size is invalid")));
                        }
                        let size = u64::from_str_radix(text, 16)
                            .map_err(|_| invalid_data("chunk size is invalid"))?;
                        if size > MAX_CHUNK_BYTES {
                            return Poll::Ready(Err(invalid_data("chunk exceeds the size limit")));
                        }
                        self.remaining = size;
                        self.state = if size == 0 {
                            ReadState::Trailers
                        } else {
                            ReadState::Data
                        };
                    }
                    None => ready!(self.as_mut().get_mut().fill(cx))?,
                },
                ReadState::Trailers => match self.try_take_line()? {
                    Some(line) if line.is_empty() => {
                        self.state = ReadState::Done;
                        return Poll::Ready(Ok(()));
                    }
                    Some(_) => {} // trailer field, ignored
                    None => ready!(self.as_mut().get_mut().fill(cx))?,
                },
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for HttpTunnel<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // Best-effort flush of whatever is already queued; retain on
        // backpressure instead of failing the frame.
        let _ = self.as_mut().get_mut().flush_pending(cx);
        if self.pending_len() > MAX_PENDING_WRITE_BYTES {
            // Inner stream is not draining; wait for writability. The pending
            // buffer is bounded by the previous write plus this cap.
            return Poll::Pending;
        }
        if self.terminator_queued {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "chunked body is already terminated",
            )));
        }
        let header = format!("{:x}\r\n", buf.len());
        self.pending.extend_from_slice(header.as_bytes());
        self.pending.extend_from_slice(buf);
        self.pending.extend_from_slice(b"\r\n");
        let _ = self.as_mut().get_mut().flush_pending(cx);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.as_mut().get_mut().flush_pending(cx))?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.terminator_queued {
            ready!(self.as_mut().get_mut().flush_pending(cx))?;
            self.pending.extend_from_slice(b"0\r\n\r\n");
            self.terminator_queued = true;
        }
        ready!(self.as_mut().get_mut().flush_pending(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[tokio::test]
    async fn tunnel_round_trips_arbitrary_chunks_both_ways() {
        let (client, server) = duplex(1024);
        let (mut client_tunnel, mut server_tunnel) =
            (HttpTunnel::new(client), HttpTunnel::new(server));
        let payload: Vec<u8> = (0..50_000u32).flat_map(|n| n.to_le_bytes()).collect();
        let expected = payload.clone();
        let writer = tokio::spawn(async move {
            for slice in payload.chunks(997) {
                client_tunnel.write_all(slice).await.unwrap();
            }
            client_tunnel.flush().await.unwrap();
            client_tunnel.shutdown().await.unwrap();
        });
        let mut received = Vec::new();
        server_tunnel.read_to_end(&mut received).await.unwrap();
        writer.await.unwrap();
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn tunnel_preserves_frame_boundaries_and_ends_cleanly() {
        let (client, server) = duplex(64);
        let (mut client_tunnel, mut server_tunnel) =
            (HttpTunnel::new(client), HttpTunnel::new(server));
        let writer = tokio::spawn(async move {
            client_tunnel.write_all(b"{\"a\":1}\n").await.unwrap();
            client_tunnel.flush().await.unwrap();
            client_tunnel.write_all(b"{\"b\":2}\n").await.unwrap();
            client_tunnel.flush().await.unwrap();
            client_tunnel.shutdown().await.unwrap();
        });
        let mut first = vec![0u8; 8];
        server_tunnel.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"{\"a\":1}\n");
        let mut second = vec![0u8; 8];
        server_tunnel.read_exact(&mut second).await.unwrap();
        assert_eq!(&second, b"{\"b\":2}\n");
        let mut tail = Vec::new();
        server_tunnel.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn truncated_or_malformed_bodies_error() {
        let (mut client, server) = duplex(1024);
        let mut tunnel = HttpTunnel::new(server);
        // Declares 10 data bytes but closes after 3 and no CRLF.
        client.write_all(b"a\r\nabc").await.unwrap();
        client.shutdown().await.unwrap();
        let mut out = Vec::new();
        assert!(tunnel.read_to_end(&mut out).await.is_err());

        let (mut client, server) = duplex(1024);
        let mut tunnel = HttpTunnel::new(server);
        client.write_all(b"zz\r\n").await.unwrap();
        let mut out = Vec::new();
        assert!(tunnel.read_to_end(&mut out).await.is_err());
    }

    #[tokio::test]
    async fn head_parsing_is_strict() {
        let (client, server) = duplex(1024);
        let mut reader = BufReader::new(server);
        let writer = tokio::spawn(async move {
            let mut client = client;
            client
                .write_all(b"POST /rpc HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\n\r\nextra")
                .await
                .unwrap();
            client.shutdown().await.unwrap();
        });
        let head = read_head(&mut reader, MAX_HEAD_BYTES).await.unwrap();
        assert_eq!(parse_request_line(&head).unwrap(), ("POST", "/rpc"));
        assert_eq!(head.header("authorization"), Some("Bearer t"));
        // Bytes after the head stay buffered for the tunnel.
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"extra");
        writer.await.unwrap();

        // A head with no request-line shape parses structurally but is refused
        // by the request-line validator.
        let bare = parse_head(b"only-one-line\r\n\r\n").unwrap();
        assert!(parse_request_line(&bare).is_err());
        assert!(parse_head(b"HTTP/1.1 200 OK\r\nbad header\r\n\r\n").is_err());
        let status = parse_head(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n").unwrap();
        assert_eq!(parse_status_line(&status).unwrap(), 401);
    }
}
