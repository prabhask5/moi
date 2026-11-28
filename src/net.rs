//! Outbound HTTP(S): the share bucket now; Uptime Kuma pushes, probes and the R2 guard later.
//!
//! A blocking client over connections from `host::Host` (so tests answer requests themselves), with
//! TLS by rustls. Trust anchors are the system's CA bundle, read through the host — no certificates
//! are compiled in, and a fixture can bring its own.

pub(crate) mod http1;
mod probe;

pub use probe::{Failure, Probe, probe, seconds};

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};

use crate::host::{Conn, Host};

/// Where distributions keep the CA bundle.
const CA_BUNDLES: [&str; 5] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
];

/// A parsed `http://` or `https://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub target: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Self, String> {
        let (https, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
            (Some(rest), _) => (true, rest),
            (_, Some(rest)) => (false, rest),
            _ => return Err(format!("not an http(s) URL: {url}")),
        };
        let (authority, target) = match rest.find(['/', '?']) {
            Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
            Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
            None => (rest, "/".to_string()),
        };
        // credentials in a URL are never sent anywhere by this client
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && !port.is_empty() => {
                (host, port.parse().map_err(|_| format!("bad port in {url}"))?)
            }
            _ => (authority, if https { 443 } else { 80 }),
        };
        if host.is_empty() || host.starts_with(':') {
            return Err(format!("no host in {url}"));
        }
        Ok(Self { https, host: host.to_string(), port, target })
    }

    /// `host` or `host:port` when the port is not the scheme's default: the `Host` header.
    pub fn authority(&self) -> String {
        if self.port == if self.https { 443 } else { 80 } {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// A finished response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Lower-cased names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A TLS session over a host connection.
struct Tls {
    stream: rustls::StreamOwned<rustls::ClientConnection, Box<dyn Conn>>,
}

impl std::fmt::Debug for Tls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tls")
    }
}

impl Read for Tls {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.stream.read(buf) {
            // a peer that closes without close_notify after a complete response is not an error here:
            // HTTP framing (Content-Length / chunked) decides whether the body is complete
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
            other => other,
        }
    }
}

impl Write for Tls {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl Conn for Tls {
    fn set_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.sock.set_timeout(timeout)
    }
}

/// An HTTP client bound to a host.
#[derive(Debug, Clone)]
pub struct Client {
    host: Arc<dyn Host>,
    timeout: Duration,
}

impl Client {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self { host, timeout: Duration::from_secs(30) }
    }

    /// How long connecting and each read or write may take (default 30 s).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn tls_config(&self) -> io::Result<Arc<rustls::ClientConfig>> {
        let mut roots = rustls::RootCertStore::empty();
        for bundle in CA_BUNDLES {
            if let Ok(pem) = self.host.read(Path::new(bundle)) {
                let certs: Vec<CertificateDer<'static>> =
                    CertificateDer::pem_slice_iter(&pem).filter_map(Result::ok).collect();
                roots.add_parsable_certificates(certs);
                if !roots.is_empty() {
                    break;
                }
            }
        }
        if roots.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no CA certificates found (looked for /etc/ssl/certs/ca-certificates.crt and the usual alternatives): install ca-certificates",
            ));
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| io::Error::other(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Arc::new(config))
    }

    /// Put TLS on a connection: the handshake is done here, so its failures are not mistaken for
    /// failures of the request. (A fixture connection plays the TLS layer itself.)
    pub(crate) fn secure(&self, mut conn: Box<dyn Conn>, server: &str) -> io::Result<Box<dyn Conn>> {
        if let Some(outcome) = conn.fake_tls() {
            return outcome.map(|()| conn);
        }
        let name = ServerName::try_from(server.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let mut session =
            rustls::ClientConnection::new(self.tls_config()?, name).map_err(|e| io::Error::other(e.to_string()))?;
        while session.is_handshaking() {
            session.complete_io(&mut conn)?;
        }
        Ok(Box::new(Tls { stream: rustls::StreamOwned::new(session, conn) }))
    }

    /// Send one request and read the whole response. Redirects are not followed.
    pub fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> io::Result<Response> {
        self.send_as(method, url, None, headers, body)
    }

    /// Like [`Client::send`], naming another site in the `Host` header than the one connected to (asking
    /// a reverse proxy on this machine for one of its sites).
    pub fn send_as(
        &self,
        method: &str,
        url: &str,
        site: Option<&str>,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> io::Result<Response> {
        let url = Url::parse(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut conn = self.host.connect_tcp(&url.host, url.port, self.timeout)?;
        conn.set_timeout(Some(self.timeout))?;
        if url.https {
            conn = self.secure(conn, &url.host)?;
        }
        let authority = url.authority();
        let host = site.unwrap_or(&authority);
        let request = http1::Request { method, target: &url.target, host, headers, body, upgrade: false };
        let response = http1::request(conn, &request)?;
        let (status, headers) = (response.status, response.headers.clone());
        Ok(Response { status, headers, body: response.bytes()? })
    }
}

/// Why a WebSocket did not deliver its first message: where it stopped, and what was said there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeError {
    /// `connect`, `tls`, `http`, `websocket` or `message`.
    pub stage: &'static str,
    pub error: String,
}

impl Client {
    /// Open a WebSocket and return the first binary or text message the server sends — enough to
    /// see that what answers behind a tunnel is an SSH server (16 § Verify). The
    /// `Sec-WebSocket-Accept` value is not checked (that would need SHA-1): the caller judges the
    /// message.
    pub fn websocket_first_message(&self, url: &str) -> Result<Vec<u8>, HandshakeError> {
        self.websocket_first_bytes(url, 1)
    }

    /// Like [`Client::websocket_first_message`], reading on until at least `enough` bytes have come:
    /// a WebSocket that carries a byte stream cuts it into messages wherever it likes.
    pub fn websocket_first_bytes(&self, url: &str, enough: usize) -> Result<Vec<u8>, HandshakeError> {
        use std::io::Read;
        let fail = |stage: &'static str| move |e: io::Error| HandshakeError { stage, error: e.to_string() };
        let url = Url::parse(url).map_err(|error| HandshakeError { stage: "connect", error })?;
        let mut conn = self.host.connect_tcp(&url.host, url.port, self.timeout).map_err(fail("connect"))?;
        conn.set_timeout(Some(self.timeout)).map_err(fail("connect"))?;
        if url.https {
            conn = self.secure(conn, &url.host).map_err(fail("tls"))?;
        }
        let nonce = self.host.random_bytes(16).map_err(fail("http"))?;
        let headers = [
            ("Upgrade".to_string(), "websocket".to_string()),
            ("Sec-WebSocket-Version".to_string(), "13".to_string()),
            ("Sec-WebSocket-Key".to_string(), crate::context::share::base64(&nonce)),
        ];
        let authority = url.authority();
        let request = http1::Request {
            method: "GET",
            target: &url.target,
            host: &authority,
            headers: &headers,
            body: None,
            upgrade: true,
        };
        let response = http1::request(conn, &request).map_err(fail("http"))?;
        if response.status != 101 {
            return Err(HandshakeError {
                stage: "websocket",
                error: format!("answered {} instead of switching protocols", response.status),
            });
        }
        // one frame: flags and opcode, length (7 bits, or 16, or 64), payload. Servers do not mask.
        let mut body = response.into_body();
        let mut read = |n: usize| -> Result<Vec<u8>, HandshakeError> {
            let mut buf = vec![0_u8; n];
            body.read_exact(&mut buf).map_err(fail("message"))?;
            Ok(buf)
        };
        let mut collected = Vec::new();
        loop {
            let head = read(2)?;
            let (opcode, masked) = (head[0] & 0x0f, head[1] & 0x80 != 0);
            let len = match head[1] & 0x7f {
                126 => usize::from(u16::from_be_bytes([read(1)?[0], read(1)?[0]])),
                127 => {
                    let wide = read(8)?;
                    usize::try_from(u64::from_be_bytes([
                        wide[0], wide[1], wide[2], wide[3], wide[4], wide[5], wide[6], wide[7],
                    ]))
                    .unwrap_or(usize::MAX)
                }
                short => usize::from(short),
            };
            // a banner is a line; nothing here needs more than this
            if len > 64 * 1024 {
                return Err(HandshakeError { stage: "message", error: format!("a first message of {len} bytes") });
            }
            let key = if masked { read(4)? } else { vec![0; 4] };
            let mut payload = read(len)?;
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= key[i % 4];
            }
            match opcode {
                0..=2 => {
                    collected.extend(payload);
                    if collected.len() >= enough {
                        return Ok(collected);
                    }
                }
                8 => {
                    return Err(HandshakeError {
                        stage: "message",
                        error: "the server closed the connection".to_string(),
                    });
                }
                // pings and pongs are not what is waited for
                _ => {}
            }
        }
    }
}

/// Percent-encode a path segment or query value (everything but unreserved characters).
pub fn encode(value: &str) -> String {
    http1::encode(value)
}
