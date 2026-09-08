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
use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::Duration;

use crate::interrupt::{self, Watch};

/// How long to wait for the TCP connection. Slirp answers quickly or not at
/// all, so a stuck connect means no network, and the turn should say so
/// rather than hang.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a read may block. A streaming response ticks over far more often
/// than this while the model works; a silence this long is a dead connection.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// What a request travels over: TLS to an API on the internet, or plain TCP
/// to a model server on the other side of the machine.
///
/// Both exist because both are real. The hosted backend talks to a public API
/// and must be encrypted; the local backend talks to a server on the same
/// host, over a loopback the packets never leave, where TLS would buy nothing
/// and cost a certificate nobody can issue for `10.0.2.2`.
enum Transport {
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    Plain(TcpStream),
}

/// The connection, and the one thing that may cut a read on it short.
///
/// A model's answer streams for seconds, and for all of them this is the
/// only socket being read. With a [`Watch`] set, every read that would block
/// waits on the watched descriptor as well, and the watch firing comes back
/// as [`interrupt::interrupted`] instead of bytes: the workspace changed, and
/// the rest of this answer is about a workspace that no longer exists.
struct Wire {
    transport: Transport,
    watch: Option<Watch>,
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(watch) = self.watch {
            match &mut self.transport {
                // Plaintext rustls has already decrypted is handed over
                // without touching the socket; only a read that would go to
                // the socket waits on the watch too.
                Transport::Tls(stream) => match stream.conn.reader().read(buf) {
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        watch.wait(stream.sock.as_fd(), READ_TIMEOUT)?;
                    }
                    other => return other,
                },
                Transport::Plain(stream) => watch.wait(stream.as_fd(), READ_TIMEOUT)?,
            }
        }
        match &mut self.transport {
            Transport::Tls(stream) => stream.read(buf),
            Transport::Plain(stream) => stream.read(buf),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.transport {
            Transport::Tls(stream) => stream.write(buf),
            Transport::Plain(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.transport {
            Transport::Tls(stream) => stream.flush(),
            Transport::Plain(stream) => stream.flush(),
        }
    }
}

/// The connection to one host, and the reader buffered over it.
type Stream = BufReader<Wire>;

/// A client for one host, which keeps its connection between requests.
///
/// This exists because of what an agentic loop actually does: a turn is not
/// one request, it is one request per tool-using exchange, ten of them in a
/// busy turn, back to back and seconds apart. Connecting each time pays DNS,
/// a TCP handshake and a TLS handshake per exchange for a connection that was
/// alive and idle a moment earlier. The TLS configuration is built once and
/// shared, so rustls can resume a session even when a connection genuinely
/// has to be remade.
pub struct Client {
    host: String,
    port: u16,
    /// The TLS configuration, or `None` for a plain connection.
    tls: Option<Arc<rustls::ClientConfig>>,
    /// The live connection, when the last response left one usable.
    idle: Option<Stream>,
    /// What may cut a response short. Set on every connection this client
    /// reads from, kept or fresh.
    watch: Option<Watch>,
}

impl Client {
    /// A client that speaks HTTPS to a host on port 443.
    pub fn https(host: &str) -> io::Result<Client> {
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
        Ok(Client {
            host: host.to_owned(),
            port: 443,
            tls: Some(Arc::new(config)),
            idle: None,
            watch: None,
        })
    }

    /// A client that speaks plain HTTP to a host and port.
    pub fn http(host: &str, port: u16) -> Client {
        Client { host: host.to_owned(), port, tls: None, idle: None, watch: None }
    }

    /// Set, or clear, the descriptor whose readability ends a response.
    pub fn watch(&mut self, watch: Option<Watch>) {
        self.watch = watch;
    }

    /// One HTTPS POST. Returns the status and a reader over the decoded body.
    pub fn post(
        &mut self,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> io::Result<Response> {
        let host = authority(&self.host, self.port, self.tls.is_some());
        let request = head_for(&host, path, headers, body.len());

        // A kept connection the server has since timed out fails on the write
        // or inside the response head and looks like nothing else, so one
        // retry on a fresh connection is the whole of what recovery means
        // here. The request is not resent over a connection that answered:
        // only over one that never spoke at all. And not over one the watch
        // cut short, which is not a fault in the connection.
        if let Some(mut stream) = self.idle.take() {
            stream.get_mut().watch = self.watch;
            match send(stream, request.as_bytes(), body) {
                Ok(response) => return Ok(response),
                Err(err) if interrupt::is_interrupted(&err) => return Err(err),
                Err(_) => {}
            }
        }
        let mut fresh = BufReader::new(self.connect()?);
        fresh.get_mut().watch = self.watch;
        send(fresh, request.as_bytes(), body)
    }

    /// Keep the connection under a finished response, if it can be kept.
    ///
    /// A response abandoned half-read cannot be: the next request would read
    /// this one's tail and call it a status line.
    pub fn recycle(&mut self, response: Response) {
        self.idle = response.keep_alive.then(|| response.body.drained()).flatten();
    }

    /// The stream to the host, ready for bytes.
    fn connect(&self) -> io::Result<Wire> {
        let address = (self.host.as_str(), self.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::other(format!("{} did not resolve", self.host)))?;
        let tcp = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
        tcp.set_read_timeout(Some(READ_TIMEOUT))?;
        tcp.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        // Nagle's algorithm holds a small write back waiting for company. A
        // request is one write and then a wait for the answer, so there is
        // never any company and the delay is pure.
        let _ = tcp.set_nodelay(true);

        let Some(config) = &self.tls else {
            return Ok(Wire { transport: Transport::Plain(tcp), watch: None });
        };
        let name = rustls::pki_types::ServerName::try_from(self.host.clone())
            .map_err(|_| io::Error::other(format!("{} is not a valid server name", self.host)))?;
        let connection =
            rustls::ClientConnection::new(Arc::clone(config), name).map_err(io::Error::other)?;
        Ok(Wire {
            transport: Transport::Tls(Box::new(rustls::StreamOwned::new(connection, tcp))),
            watch: None,
        })
    }

    /// The host and port, for an error message that says where it was trying
    /// to go.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// The request head, up to but not including the body.
fn head_for(host: &str, path: &str, headers: &[(&str, &str)], length: usize) -> String {
    // No `connection` header: HTTP/1.1 keeps the connection alive unless one
    // end says otherwise, and this end never wants to.
    let mut request =
        format!("POST {path} HTTP/1.1\r\nhost: {host}\r\ncontent-length: {length}\r\n");
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request
}

/// Write one request down a stream and read its head back.
fn send(mut stream: Stream, request: &[u8], body: &[u8]) -> io::Result<Response> {
    stream.get_mut().write_all(request)?;
    stream.get_mut().write_all(body)?;
    stream.get_mut().flush()?;

    let (status, headers) = read_head(&mut stream)?;
    // A server may decline to keep the connection whatever the client wants.
    let keep_alive = !header(&headers, "connection")
        .is_some_and(|value| value.to_ascii_lowercase().contains("close"));

    let body = if header(&headers, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        Body::Chunked(ChunkedReader::new(stream))
    } else if let Some(length) = header(&headers, "content-length").and_then(|v| v.parse().ok()) {
        Body::Sized { reader: stream, remaining: length }
    } else {
        // No framing declared: the body runs to the close of the connection,
        // whatever either end said about keeping it.
        Body::ToEnd(stream)
    };

    Ok(Response { status, keep_alive, body })
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
    /// Whether the server is willing to keep the connection afterwards.
    keep_alive: bool,
    body: Body<Stream>,
}

/// The `host` header for a client. A port that is not the scheme's default
/// belongs in it, because a server routing by name needs to be told.
fn authority(host: &str, port: u16, tls: bool) -> String {
    if (tls && port == 443) || (!tls && port == 80) {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    }
}

impl Response {
    /// The whole body, for the small responses: errors, and anything not
    /// streamed.
    pub fn read_to_string(&mut self) -> io::Result<String> {
        let mut text = String::new();
        Read::read_to_string(self, &mut text)?;
        Ok(text)
    }

    /// Read whatever framing follows a payload a caller stopped reading at,
    /// so the connection underneath can be kept.
    ///
    /// A server-sent event stream ends at an event the reader recognises,
    /// with a few bytes of chunk framing still behind it; without those the
    /// response is half-read and the connection has to be thrown away. A body
    /// whose end *is* the connection's close is left alone: there is nothing
    /// to read there that would not be a wait for the peer to hang up.
    pub fn drain(&mut self) {
        if matches!(self.body, Body::ToEnd(_)) {
            return;
        }
        let mut scratch = [0u8; 256];
        while matches!(self.body.read(&mut scratch), Ok(got) if got > 0) {}
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

impl<R: Read> Body<R> {
    /// The reader under a body that has been read to its end, or `None` when
    /// bytes of this response are still on the wire.
    fn drained(self) -> Option<R> {
        match self {
            Body::Chunked(reader) => reader.done.then_some(reader.inner),
            Body::Sized { reader, remaining } => (remaining == 0).then_some(reader),
            // The close of the connection is the end of this body, so there
            // is by definition nothing left to keep.
            Body::ToEnd(_) => None,
        }
    }
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
    fn a_body_read_to_its_end_gives_its_connection_back() {
        let wire = b"4\r\nWiki\r\n0\r\n\r\n";
        let mut body = Body::Chunked(ChunkedReader::new(Cursor::new(&wire[..])));
        let mut text = String::new();
        body.read_to_string(&mut text).unwrap();
        assert_eq!(text, "Wiki");
        assert!(body.drained().is_some());
    }

    #[test]
    fn a_half_read_body_does_not() {
        // Stopping at the payload, which is what an event reader does when it
        // meets the event it was waiting for, leaves framing on the wire. The
        // next request down that connection would read this one's tail and
        // call it a status line, so the connection has to be thrown away
        // unless something reads the rest first.
        let wire = b"4\r\nWiki\r\n0\r\n\r\n";
        let mut body = Body::Chunked(ChunkedReader::new(Cursor::new(&wire[..])));
        body.read_exact(&mut [0u8; 4]).unwrap();
        assert!(body.drained().is_none());

        let mut body = Body::Sized { reader: Cursor::new(&b"hello"[..]), remaining: 5 };
        body.read_exact(&mut [0u8; 2]).unwrap();
        assert!(body.drained().is_none());

        // A body whose end is the connection's close can never be kept.
        assert!(Body::ToEnd(Cursor::new(&b""[..])).drained().is_none());
    }

    #[test]
    fn requests_ask_to_keep_the_connection() {
        let head = head_for("api.example", "/v1/messages", &[("x-api-key", "k")], 12);
        assert!(head.starts_with("POST /v1/messages HTTP/1.1\r\n"));
        assert!(head.contains("host: api.example\r\n"));
        assert!(head.contains("content-length: 12\r\n"));
        assert!(head.contains("x-api-key: k\r\n"));
        // HTTP/1.1 keeps the connection unless one end says otherwise, and
        // this end never wants to.
        assert!(!head.to_ascii_lowercase().contains("connection:"));
        assert!(head.ends_with("\r\n\r\n"));
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
