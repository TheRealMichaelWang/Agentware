//! A minimal HTTPS client: one host, one route, HTTP/1.1.
//!
//! The agent needs exactly one thing from the network: `POST` a JSON body to
//! an API host and read the response back, possibly as a long-lived streamed
//! body. A general HTTP library would bring async runtimes and connection
//! pools this process has no use for; a per-turn worker makes a handful of
//! requests and exits. So the protocol is written out here, in full, the way
//! the supervisor's wire format is: small enough to read, and doing nothing
//! it does not say.
//!
//! TLS is rustls with the ring provider, and the CA bundle is webpki's,
//! compiled in, because the system image carries no `/etc/ssl`. DNS is musl's
//! resolver, which reads the `/etc/resolv.conf` the supervisor writes at
//! boot.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// How long to wait for the TCP connection. Slirp answers quickly or not at
/// all, so a stuck connect means no network, and the turn should say so
/// rather than hang.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a read may block. A streaming response ticks over far more often
/// than this while the model works; a silence this long is a dead connection.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// One HTTPS POST. Returns the status and a reader over the decoded body.
pub fn post(host: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> io::Result<Response> {
    let stream = connect(host)?;

    let mut request = format!(
        "POST {path} HTTP/1.1\r\nhost: {host}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");

    let mut stream = stream;
    stream.write_all(request.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let (status, headers) = read_head(&mut reader)?;

    let body: Body<_> = if header(&headers, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        Body::Chunked(ChunkedReader::new(reader))
    } else if let Some(length) = header(&headers, "content-length").and_then(|v| v.parse().ok()) {
        Body::Sized { reader, remaining: length }
    } else {
        // No framing declared: the body runs to the close of the connection,
        // which `connection: close` asked for anyway.
        Body::ToEnd(reader)
    };

    Ok(Response { status, body })
}

/// The TLS stream to a host, ready for bytes.
fn connect(host: &str) -> io::Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    let address = (host, 443u16)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other(format!("{host} did not resolve to an address")))?;
    let tcp = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
    tcp.set_read_timeout(Some(READ_TIMEOUT))?;
    tcp.set_write_timeout(Some(CONNECT_TIMEOUT))?;

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(io::Error::other)?
    .with_root_certificates(roots)
    .with_no_client_auth();

    let name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| io::Error::other(format!("{host} is not a valid server name")))?;
    let connection =
        rustls::ClientConnection::new(Arc::new(config), name).map_err(io::Error::other)?;
    Ok(rustls::StreamOwned::new(connection, tcp))
}

/// The status line and headers, up to the blank line.
fn read_head<R: BufRead>(reader: &mut R) -> io::Result<(u16, Vec<(String, String)>)> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::other(format!("not an HTTP status line: {}", line.trim())))?;

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::other("connection closed inside the response head"));
        }
        let line = line.trim_end();
        if line.is_empty() {
            return Ok((status, headers));
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// A response: the status, and the body as a reader that decodes the wire
/// framing, so a caller can stream server-sent events line by line.
pub struct Response {
    pub status: u16,
    body: Body<BufReader<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>>,
}

impl Response {
    /// The whole body, for the small responses: errors, and anything not
    /// streamed.
    pub fn read_to_string(&mut self) -> io::Result<String> {
        let mut text = String::new();
        Read::read_to_string(self, &mut text)?;
        Ok(text)
    }
}

impl Read for Response {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.body.read(buf)
    }
}

/// The three framings an HTTP/1.1 body arrives in.
enum Body<R: Read> {
    Chunked(ChunkedReader<R>),
    Sized { reader: R, remaining: usize },
    ToEnd(R),
}

impl<R: Read> Read for Body<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Body::Chunked(reader) => reader.read(buf),
            Body::Sized { reader, remaining } => {
                if *remaining == 0 {
                    return Ok(0);
                }
                let take = buf.len().min(*remaining);
                let got = reader.read(&mut buf[..take])?;
                *remaining -= got;
                Ok(got)
            }
            Body::ToEnd(reader) => match reader.read(buf) {
                // A peer that closes without a close_notify alert looks like
                // an unexpected EOF to rustls; with `connection: close` and no
                // declared length, the close *is* the end of the body.
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                other => other,
            },
        }
    }
}

/// Decodes `Transfer-Encoding: chunked`: a hex length line, that many bytes,
/// CRLF, repeated; a zero length ends the body, followed by trailers this
/// client ignores.
///
/// Generic over the underlying reader so the decoding can be tested against
/// bytes in memory rather than a live socket.
pub struct ChunkedReader<R: Read> {
    inner: R,
    /// Bytes left in the current chunk.
    remaining: usize,
    done: bool,
}

impl<R: Read> ChunkedReader<R> {
    pub fn new(inner: R) -> Self {
        ChunkedReader { inner, remaining: 0, done: false }
    }

    /// One byte, or `None` at a clean end of stream.
    fn byte(&mut self) -> io::Result<Option<u8>> {
        let mut byte = [0u8; 1];
        loop {
            match self.inner.read(&mut byte) {
                Ok(0) => return Ok(None),
                Ok(_) => return Ok(Some(byte[0])),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => Err(err)?,
            }
        }
    }

    /// A line up to CRLF, as bytes without the terminator.
    fn line(&mut self) -> io::Result<Vec<u8>> {
        let mut line = Vec::new();
        while let Some(byte) = self.byte()? {
            if byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            line.push(byte);
        }
        Err(io::Error::other("connection closed inside a chunked body"))
    }

    /// Read the next chunk-size line and set up `remaining`.
    fn next_chunk(&mut self) -> io::Result<()> {
        let line = self.line()?;
        // A chunk-size line may carry extensions after a semicolon; only the
        // hex count matters.
        let text = String::from_utf8_lossy(&line);
        let digits = text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(digits, 16)
            .map_err(|_| io::Error::other(format!("not a chunk size: {digits:?}")))?;
        if size == 0 {
            // Trailers until a blank line, then done.
            while !self.line()?.is_empty() {}
            self.done = true;
        }
        self.remaining = size;
        Ok(())
    }
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.next_chunk()?;
            if self.done {
                return Ok(0);
            }
        }
        let take = buf.len().min(self.remaining);
        let got = self.inner.read(&mut buf[..take])?;
        if got == 0 {
            return Err(io::Error::other("connection closed inside a chunk"));
        }
        self.remaining -= got;
        if self.remaining == 0 {
            // The CRLF that closes every chunk.
            let (a, b) = (self.byte()?, self.byte()?);
            if a != Some(b'\r') || b != Some(b'\n') {
                return Err(io::Error::other("a chunk did not end in CRLF"));
            }
        }
        Ok(got)
    }
}

/// Server-sent events off any reader: `event:` and `data:` lines accumulate
/// until a blank line dispatches them.
pub struct SseReader<R: Read> {
    lines: BufReader<R>,
}

/// One event. `data` joins multi-line payloads with newlines, per the SSE
/// format, though the API sends one line per event.
#[derive(Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

impl<R: Read> SseReader<R> {
    pub fn new(inner: R) -> Self {
        SseReader { lines: BufReader::new(inner) }
    }

    /// The next event, or `None` when the stream ends.
    pub fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        let mut event = String::new();
        let mut data: Option<String> = None;

        loop {
            let mut line = String::new();
            if self.lines.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            let line = line.trim_end_matches(['\r', '\n']);

            if line.is_empty() {
                if let Some(data) = data {
                    return Ok(Some(SseEvent { event, data }));
                }
                event.clear();
                continue;
            }
            if let Some(name) = line.strip_prefix("event:") {
                event = name.trim_start().to_owned();
            } else if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.strip_prefix(' ').unwrap_or(payload);
                match &mut data {
                    Some(text) => {
                        text.push('\n');
                        text.push_str(payload);
                    }
                    None => data = Some(payload.to_owned()),
                }
            }
            // Comments (`: keepalive`) and fields this client does not use
            // (`id:`, `retry:`) fall through.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn chunked_reassembles() {
        // Sizes in hex, an extension to ignore, and a trailer to skip.
        let wire = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\nE\r\n in\r\n\r\nchunks.\r\n0\r\nx-trailer: 1\r\n\r\n";
        let mut reader = ChunkedReader::new(Cursor::new(&wire[..]));
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        assert_eq!(text, "Wikipedia in\r\n\r\nchunks.");
        // Reading past the end stays at the end.
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn chunked_refuses_a_truncated_body() {
        let wire = b"A\r\nhalf";
        let mut reader = ChunkedReader::new(Cursor::new(&wire[..]));
        let mut text = String::new();
        assert!(reader.read_to_string(&mut text).is_err());
    }

    #[test]
    fn sse_events_parse() {
        let wire = "event: message_start\ndata: {\"a\":1}\n\n: keepalive\n\nevent: done\ndata: one\ndata: two\n\n";
        let mut reader = SseReader::new(Cursor::new(wire.as_bytes()));
        assert_eq!(
            reader.next_event().unwrap().unwrap(),
            SseEvent { event: "message_start".into(), data: "{\"a\":1}".into() }
        );
        assert_eq!(
            reader.next_event().unwrap().unwrap(),
            SseEvent { event: "done".into(), data: "one\ntwo".into() }
        );
        assert!(reader.next_event().unwrap().is_none());
    }

    #[test]
    fn heads_parse() {
        let wire = "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 5\r\n\r\nbody";
        let mut reader = std::io::BufReader::new(Cursor::new(wire.as_bytes()));
        let (status, headers) = read_head(&mut reader).unwrap();
        assert_eq!(status, 429);
        assert_eq!(header(&headers, "retry-after"), Some("5"));
        assert_eq!(header(&headers, "content-type"), Some("application/json"));
    }
}
