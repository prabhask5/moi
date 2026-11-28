//! A minimal HTTP/1.1 client: one request per connection over a stream the host provides (the Docker
//! engine's unix socket, a TCP connection, a TLS session). Sync on purpose — moi makes a handful of
//! short requests per run, and the daemon (M6) follows the Docker event stream on its own thread; an
//! async HTTP stack would be most of the dependency tree for no gain (docs/spec/14-security.md).

use std::io::{self, BufRead, BufReader, Read, Write};

use crate::host::Conn;

/// A response whose body has not been read yet.
pub(crate) struct Response {
    pub status: u16,
    /// Lower-cased names.
    pub headers: Vec<(String, String)>,
    body: Body,
}

/// The body, decoded from its transfer encoding as it is read.
pub(crate) struct Body {
    reader: BufReader<Box<dyn Conn>>,
    framing: Framing,
}

enum Framing {
    Length(u64),
    /// Bytes left in the current chunk; `None` before the first chunk header and after the last chunk.
    Chunked {
        left: u64,
        done: bool,
    },
    /// Until the peer closes (upgraded/hijacked streams, HTTP/1.0-style bodies).
    Eof,
}

fn protocol(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// What to send. `target` is the request target (`/path?query`), `host` the `Host` header.
pub(crate) struct Request<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub host: &'a str,
    /// Extra headers (`Content-Length`, `Host`, `Connection` are set here).
    pub headers: &'a [(String, String)],
    pub body: Option<&'a [u8]>,
    /// Ask for a protocol upgrade (Docker's raw exec stream).
    pub upgrade: bool,
}

/// Send a request and read the response head.
pub(crate) fn request(mut conn: Box<dyn Conn>, req: &Request<'_>) -> io::Result<Response> {
    let (method, body) = (req.method, req.body);
    let mut head =
        format!("{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: moi\r\nAccept: */*\r\n", req.target, req.host);
    if req.upgrade {
        // Docker's raw streams unless the caller names another protocol (a WebSocket)
        head.push_str("Connection: Upgrade\r\n");
        if !req.headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("upgrade")) {
            head.push_str("Upgrade: tcp\r\n");
        }
    } else {
        head.push_str("Connection: close\r\n");
    }
    for (name, value) in req.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    } else if method != "GET" && method != "HEAD" {
        head.push_str("Content-Length: 0\r\n");
    }
    head.push_str("\r\n");
    conn.write_all(head.as_bytes())?;
    if let Some(body) = body {
        conn.write_all(body)?;
    }
    conn.flush()?;

    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let status = match (parts.next(), parts.next().and_then(|s| s.parse::<u16>().ok())) {
        (Some(version), Some(status)) if version.starts_with("HTTP/1.") => status,
        _ => return Err(protocol(format!("not an HTTP response: {}", line.trim_end()))),
    };
    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(protocol("connection closed in the response headers"));
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    let framing = if status == 101 || method == "HEAD" && status < 300 {
        Framing::Eof
    } else if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        Framing::Chunked { left: 0, done: false }
    } else if let Some(len) = header("content-length") {
        Framing::Length(len.parse().map_err(|_| protocol("bad Content-Length"))?)
    } else if status == 204 || status == 304 {
        Framing::Length(0)
    } else {
        Framing::Eof
    };
    Ok(Response { status, headers, body: Body { reader, framing } })
}

impl Response {
    /// The whole body.
    pub fn bytes(mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.body.read_to_end(&mut out)?;
        Ok(out)
    }

    /// The body as a stream (events, logs, exec output).
    pub fn into_body(self) -> Body {
        self.body
    }
}

impl Body {
    /// The connection itself, for writing to an upgraded stream (the standard input of an exec).
    pub fn conn(&mut self) -> &mut Box<dyn Conn> {
        self.reader.get_mut()
    }

    /// Change the read timeout of the underlying connection (`None` blocks forever, for event streams).
    pub fn set_timeout(&mut self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.reader.get_mut().set_timeout(timeout)
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match &mut self.framing {
            Framing::Eof => self.reader.read(buf),
            Framing::Length(left) => {
                if *left == 0 {
                    return Ok(0);
                }
                let want = buf.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
                let n = self.reader.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed mid-body"));
                }
                *left -= n as u64;
                Ok(n)
            }
            Framing::Chunked { left, done } => {
                if *done {
                    return Ok(0);
                }
                if *left == 0 {
                    let mut line = String::new();
                    self.reader.read_line(&mut line)?;
                    if line.trim().is_empty() {
                        // the CRLF that ends the previous chunk; the size line follows
                        line.clear();
                        self.reader.read_line(&mut line)?;
                    }
                    let size = line.trim().split(';').next().unwrap_or_default();
                    if size.is_empty() {
                        // closed between chunks: how a daemon ends a stream it was following
                        *done = true;
                        return Ok(0);
                    }
                    *left = u64::from_str_radix(size, 16).map_err(|_| protocol(format!("bad chunk size: {size}")))?;
                    if *left == 0 {
                        *done = true;
                        return Ok(0);
                    }
                }
                let want = buf.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
                let n = self.reader.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed mid-chunk"));
                }
                *left -= n as u64;
                Ok(n)
            }
        }
    }
}

/// Percent-encode a query value (everything but unreserved characters).
pub(crate) fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Split Docker's multiplexed stream (8-byte frame headers: stream type, 3 zero bytes, big-endian
/// length) into stdout and stderr. A stream that does not start with a valid frame header is a TTY
/// stream and is returned whole as stdout.
pub(crate) fn demux(raw: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let framed = |b: &[u8]| b.len() >= 8 && b[0] <= 2 && b[1..4] == [0, 0, 0];
    if !raw.is_empty() && !framed(raw) {
        return (raw.to_vec(), Vec::new());
    }
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut rest = raw;
    while framed(rest) {
        let len = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
        let end = (8 + len).min(rest.len());
        if rest[0] == 2 {
            stderr.extend_from_slice(&rest[8..end])
        } else {
            stdout.extend_from_slice(&rest[8..end])
        }
        rest = &rest[end..];
    }
    (stdout, stderr)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};
    use std::time::Duration;

    use super::{Request, demux, request};
    use crate::host::Conn;

    /// A peer that answers with whatever bytes it is given.
    #[derive(Debug)]
    struct Bytes(io::Cursor<Vec<u8>>);

    impl Read for Bytes {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for Bytes {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Conn for Bytes {
        fn set_timeout(&mut self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    fn answer(bytes: &[u8]) -> io::Result<Vec<u8>> {
        let req = Request { method: "GET", target: "/", host: "peer", headers: &[], body: None, upgrade: false };
        request(Box::new(Bytes(io::Cursor::new(bytes.to_vec()))), &req)?.bytes()
    }

    #[test]
    fn a_well_formed_answer_is_read() {
        let body = answer(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n");
        assert_eq!(body.ok().as_deref(), Some(b"hello".as_slice()));
    }

    /// Whatever a peer sends — cut off, garbled, lying about lengths — is an error or a body, never a
    /// panic and never an allocation of the size the peer claims.
    #[test]
    fn no_answer_of_a_peer_can_crash_the_reader() {
        let good: &[&[u8]] = &[
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n3\r\nabc\r\n0\r\n\r\n",
            b"HTTP/1.1 101 UPGRADED\r\nConnection: Upgrade\r\n\r\n\x01\x00\x00\x00\x00\x00\x00\x02ok",
            b"HTTP/1.1 204 No Content\r\n\r\n",
        ];
        let nasty: &[&[u8]] = &[
            b"",
            b"\r\n\r\n",
            b"HTTP/1.1\r\n\r\n",
            b"HTTP/1.1 999999999999999999999 X\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551615\r\n\r\nx",
            b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\nx",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nx",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nx",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel",
            b"HTTP/1.1 200 OK\r\n: \r\n:\r\nNoColon\r\n\r\n",
            b"\xff\xfe\xfd 200 \xff\r\n\xff: \xff\r\n\r\n\xff",
        ];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut tried = 0;
        for input in good.iter().chain(nasty) {
            let _ = answer(input);
            let _ = demux(input);
            // every truncation, and a few hundred garbled copies of each
            for cut in 0..input.len() {
                let _ = answer(&input[..cut]);
                let _ = demux(&input[..cut]);
                tried += 1;
            }
            for _ in 0..300 {
                let mut bytes = input.to_vec();
                for _ in 0..=(next() % 4) {
                    if bytes.is_empty() {
                        break;
                    }
                    let at = usize::try_from(next() % bytes.len() as u64).unwrap_or(0);
                    match next() % 4 {
                        0 => bytes[at] = (next() % 256) as u8,
                        1 => {
                            bytes.remove(at);
                        }
                        2 => bytes.insert(at, b"\r\n:0 9f"[usize::try_from(next() % 7).unwrap_or(0)]),
                        _ => bytes.truncate(at),
                    }
                }
                let _ = answer(&bytes);
                let _ = demux(&bytes);
                tried += 1;
            }
        }
        assert!(tried > 4000, "{tried}");
    }
}
