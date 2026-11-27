//! A staged HTTP probe: resolve, connect, TLS, request — each stage timed and bounded, so a failure
//! names the stage it happened in (docs/spec/05-checks.md § Probe). The legacy check got this from
//! curl's `-w` timings; the messages are the ones it built from them.

use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use super::{Client, Url, http1};
use crate::host::Host;

/// Why a probe got no HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Dns,
    DnsTimeout,
    Refused,
    ConnectTimeout,
    TlsTimeout,
    Tls(String),
    /// Connected, asked, and nothing came back in time.
    NoResponse,
    /// The response started and did not finish in time.
    TooSlow,
    Dropped(String),
    Other(String),
}

/// What a probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The HTTP status, once the head of a response arrived — also when the rest then failed, which is
    /// how curl reports it (and what the legacy check judged by).
    pub status: Option<u16>,
    pub failure: Option<Failure>,
    /// The address that was tried, once the name resolved.
    pub ip: Option<String>,
    pub total: Duration,
}

/// Seconds the way curl prints them: `0.412345`.
pub fn seconds(duration: Duration) -> String {
    format!("{}.{:06}", duration.as_secs(), duration.subsec_micros())
}

impl Probe {
    /// One line saying how it went.
    pub fn message(&self) -> String {
        let total = seconds(self.total);
        let ip = self.ip.as_deref().unwrap_or("?");
        let Some(failure) = &self.failure else {
            let code = self.status.unwrap_or(0);
            let cloudflare = if (520..=530).contains(&code) { " (Cloudflare couldn't reach the origin)" } else { "" };
            return format!("HTTP {code} in {total}s{cloudflare}");
        };
        let what = match failure {
            Failure::Dns => "DNS lookup failed".to_string(),
            Failure::DnsTimeout => "timed out in DNS lookup".to_string(),
            Failure::Refused => format!("connection refused by {ip}"),
            Failure::ConnectTimeout => format!("timed out connecting to {ip}"),
            Failure::TlsTimeout => format!("timed out in TLS handshake with {ip}"),
            Failure::Tls(error) => format!("TLS/cert error: {error}"),
            Failure::NoResponse => format!("connected to {ip}, no response from origin"),
            Failure::TooSlow => "response too slow".to_string(),
            Failure::Dropped(error) => format!("connection dropped by {ip}: {error}"),
            Failure::Other(error) => format!("error: {error}"),
        };
        format!("{what} after {total}s")
    }
}

fn timed_out(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

fn dropped(error: &io::Error) -> bool {
    use io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    matches!(error.kind(), ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof)
}

/// `GET` a URL, giving up `timeout` after the start. Redirects are not followed.
pub fn probe(host: &Arc<dyn Host>, url: &str, timeout: Duration) -> Probe {
    let start = host.monotonic();
    let left = || timeout.saturating_sub(host.monotonic().saturating_sub(start));
    let (mut ip, mut status) = (None, None);
    let failure = stages(host, url, &left, &mut ip, &mut status).err();
    let total = host.monotonic().saturating_sub(start);
    Probe { status, failure, ip, total }
}

fn stages(
    host: &Arc<dyn Host>,
    url: &str,
    left: &dyn Fn() -> Duration,
    ip: &mut Option<String>,
    status: &mut Option<u16>,
) -> Result<(), Failure> {
    let url = Url::parse(url).map_err(Failure::Other)?;
    // 1. the name
    let addresses =
        host.resolve(&url.host, left()).map_err(|e| if timed_out(&e) { Failure::DnsTimeout } else { Failure::Dns })?;
    let address = addresses.first().ok_or(Failure::Dns)?.to_string();
    *ip = Some(address.clone());
    // 2. the connection
    let mut conn = host.connect_tcp(&address, url.port, left()).map_err(|e| match e.kind() {
        io::ErrorKind::ConnectionRefused => Failure::Refused,
        _ if timed_out(&e) => Failure::ConnectTimeout,
        _ => Failure::Other(e.to_string()),
    })?;
    let bound = |conn: &mut Box<dyn crate::host::Conn>| {
        conn.set_timeout(Some(left().max(Duration::from_millis(1)))).map_err(|e| Failure::Other(e.to_string()))
    };
    bound(&mut conn)?;
    // 3. TLS
    if url.https {
        conn = Client::new(Arc::clone(host))
            .secure(conn, &url.host)
            .map_err(|e| if timed_out(&e) { Failure::TlsTimeout } else { Failure::Tls(e.to_string()) })?;
        bound(&mut conn)?;
    }
    // 4. the request, up to the first byte of the answer
    let authority = url.authority();
    let request = http1::Request {
        method: "GET",
        target: &url.target,
        host: &authority,
        headers: &[],
        body: None,
        upgrade: false,
    };
    let response = http1::request(conn, &request).map_err(|e| {
        if timed_out(&e) {
            Failure::NoResponse
        } else if dropped(&e) || e.kind() == io::ErrorKind::InvalidData {
            Failure::Dropped(e.to_string())
        } else {
            Failure::Other(e.to_string())
        }
    })?;
    // 5. the rest of the answer
    *status = Some(response.status);
    let mut body = response.into_body();
    let mut sink = [0u8; 8192];
    loop {
        // each read may take what is left of the whole probe's time, not a fresh allowance
        body.set_timeout(Some(left().max(Duration::from_millis(1)))).map_err(|e| Failure::Other(e.to_string()))?;
        match body.read(&mut sink) {
            Ok(0) => return Ok(()),
            Ok(_) if left().is_zero() => return Err(Failure::TooSlow),
            Ok(_) => {}
            Err(e) if timed_out(&e) => return Err(Failure::TooSlow),
            Err(e) if dropped(&e) => return Err(Failure::Dropped(e.to_string())),
            Err(e) => return Err(Failure::Other(e.to_string())),
        }
    }
}
