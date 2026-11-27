//! A fake network for `FixtureHost`, described by the `net` file of a fixture or case: what names
//! resolve to, how each address answers a connection, and what each URL returns. Anything not listed
//! does not resolve or refuses the connection. See `tests/fixtures/README.md`.
//!
//! Time is virtual: a stage that "takes" 300 ms, a timeout that expires and `Host::sleep` all add to a
//! per-thread counter that the fixture's clocks include. Nothing really waits, timings are exact, and
//! probes running side by side do not disturb each other's measurements.

use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use super::Conn;

thread_local! {
    static SPENT: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

/// Virtual time this thread has spent waiting.
pub(crate) fn spent() -> Duration {
    SPENT.with(Cell::get)
}

/// Start this thread's virtual time over (a new fixture host begins at its own clock).
pub(crate) fn reset() {
    SPENT.with(|s| s.set(Duration::ZERO));
}

/// Let virtual time pass for this thread.
pub(crate) fn spend(duration: Duration) {
    SPENT.with(|s| s.set(s.get() + duration));
}

#[derive(Debug, Clone)]
enum Answer {
    Address(IpAddr),
    NoSuchName,
    Timeout,
}

#[derive(Debug, Clone)]
struct Dns {
    answer: Answer,
    took: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Behaviour {
    Ok,
    Refused,
    Timeout,
    TlsTimeout,
    TlsError(String),
    /// Accepts the connection, then drops it when asked something.
    Reset,
    /// Accepts the connection and never answers.
    Silent,
    /// Starts answering and never finishes.
    Slow,
}

#[derive(Debug, Clone)]
struct Tcp {
    behaviour: Behaviour,
    connect: Duration,
    tls: Duration,
    /// Until the response starts.
    first: Duration,
    /// From then until it is complete.
    rest: Duration,
}

impl Default for Tcp {
    fn default() -> Self {
        Self {
            behaviour: Behaviour::Ok,
            connect: Duration::ZERO,
            tls: Duration::ZERO,
            first: Duration::ZERO,
            rest: Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Net {
    dns: BTreeMap<String, Dns>,
    /// `ip:port` → how it answers.
    tcp: BTreeMap<String, Tcp>,
    /// `Host` header + request target → (statuses, body). Several statuses are answered in turn, the
    /// last one from then on (`502,200`: fails once, then works).
    http: BTreeMap<String, (Vec<u16>, String)>,
    /// How often each URL was asked for.
    asked: Arc<std::sync::Mutex<BTreeMap<String, usize>>>,
}

/// `0.3`, `15`, `0.004500`: seconds with up to six decimals, exactly (no floating point).
fn seconds(text: &str) -> Option<Duration> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if fraction.len() > 6 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let micros: u64 = format!("{fraction:0<6}").parse().ok()?;
    Some(Duration::from_secs(whole.parse().ok()?) + Duration::from_micros(micros))
}

impl Net {
    /// Add one line of a `net` file; later lines replace earlier ones about the same thing.
    pub fn add_line(&mut self, line: &str) -> Result<(), &'static str> {
        let mut words = line.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some("dns"), Some(name), Some(answer)) => {
                let answer = match answer {
                    "-" => Answer::NoSuchName,
                    "timeout" => Answer::Timeout,
                    ip => Answer::Address(ip.parse().map_err(|_| "`dns name <ip>|-|timeout [took=seconds]`")?),
                };
                let took = match words.next() {
                    Some(word) => word.strip_prefix("took=").and_then(seconds).ok_or("`took=<seconds>`")?,
                    None => Duration::ZERO,
                };
                self.dns.insert(name.to_string(), Dns { answer, took });
            }
            (Some("tcp"), Some(address), Some(behaviour)) => {
                let behaviour = match behaviour {
                    "ok" => Behaviour::Ok,
                    "refused" => Behaviour::Refused,
                    "timeout" => Behaviour::Timeout,
                    "tls-timeout" => Behaviour::TlsTimeout,
                    "reset" => Behaviour::Reset,
                    "silent" => Behaviour::Silent,
                    "slow" => Behaviour::Slow,
                    // the rest of the line is the error
                    "tls-error" => Behaviour::TlsError(words.by_ref().collect::<Vec<_>>().join(" ")),
                    _ => {
                        return Err(
                            "`tcp ip:port ok|refused|timeout|tls-timeout|tls-error <message>|reset|silent|slow [connect=s] [tls=s] [first=s] [rest=s]`",
                        );
                    }
                };
                let mut tcp = Tcp { behaviour, ..Tcp::default() };
                for word in words {
                    let (stage, value) = word.split_once('=').ok_or("`stage=<seconds>`")?;
                    let value = seconds(value).ok_or("`stage=<seconds>`")?;
                    match stage {
                        "connect" => tcp.connect = value,
                        "tls" => tcp.tls = value,
                        "first" => tcp.first = value,
                        "rest" => tcp.rest = value,
                        _ => return Err("a stage is connect, tls, first or rest"),
                    }
                }
                self.tcp.insert(address.to_string(), tcp);
            }
            (Some("http"), Some(url), Some(status)) => {
                if status == "-" {
                    self.http.remove(url);
                } else {
                    let status: Vec<u16> = status
                        .split(',')
                        .map(str::parse)
                        .collect::<Result<_, _>>()
                        .map_err(|_| "`http host/path <status>[,<status>…]|- [body]`")?;
                    self.http.insert(url.to_string(), (status, words.collect::<Vec<_>>().join(" ")));
                }
            }
            _ => return Err("`dns …`, `tcp …` or `http …`"),
        }
        Ok(())
    }

    pub fn resolve(&self, name: &str, timeout: Duration) -> io::Result<Vec<IpAddr>> {
        if let Ok(ip) = name.parse() {
            return Ok(vec![ip]);
        }
        let no_such_name =
            || io::Error::new(io::ErrorKind::NotFound, format!("{name} does not resolve in this fixture"));
        let dns = self.dns.get(name).ok_or_else(no_such_name)?;
        match dns.answer {
            Answer::Timeout => {
                spend(timeout);
                Err(io::Error::new(io::ErrorKind::TimedOut, format!("resolving {name} timed out")))
            }
            Answer::NoSuchName => {
                spend(dns.took);
                Err(no_such_name())
            }
            Answer::Address(ip) => {
                spend(dns.took);
                Ok(vec![ip])
            }
        }
    }

    pub fn connect(self: &Arc<Self>, host: &str, port: u16, timeout: Duration) -> io::Result<Box<dyn Conn>> {
        let address = format!("{host}:{port}");
        let refused = || io::Error::new(io::ErrorKind::ConnectionRefused, "Connection refused");
        // connecting by name goes to the address the name resolves to
        let by_name = || match self.dns.get(host).map(|dns| &dns.answer) {
            Some(Answer::Address(ip)) => self.tcp.get(&format!("{ip}:{port}")),
            _ => None,
        };
        let tcp = match self.tcp.get(&address).or_else(by_name) {
            Some(tcp) => tcp.clone(),
            // an address some `http` line is about accepts connections without being listed
            None if self.http.keys().any(|url| {
                url.starts_with(&format!("{address}/")) || port == 80 && url.starts_with(&format!("{host}/"))
            }) =>
            {
                Tcp::default()
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!(
                        "fixture host has no network: cannot connect to {address} (no `tcp` or `http` line for it in a net file)"
                    ),
                ));
            }
        };
        match tcp.behaviour {
            Behaviour::Refused => {
                spend(tcp.connect);
                Err(refused())
            }
            Behaviour::Timeout => {
                spend(timeout);
                Err(io::Error::new(io::ErrorKind::TimedOut, "connection timed out"))
            }
            _ => {
                spend(tcp.connect);
                Ok(Box::new(NetConn {
                    net: Arc::clone(self),
                    tcp,
                    timeout: None,
                    request: Vec::new(),
                    queue: None,
                    current: io::Cursor::new(Vec::new()),
                }))
            }
        }
    }
}

/// A connection to the fake network.
#[derive(Debug)]
struct NetConn {
    net: Arc<Net>,
    tcp: Tcp,
    timeout: Option<Duration>,
    request: Vec<u8>,
    /// Parts of the response still to come, each with the time it takes to arrive.
    queue: Option<VecDeque<(Duration, Vec<u8>)>>,
    current: io::Cursor<Vec<u8>>,
}

impl NetConn {
    fn time_out(&self) -> io::Error {
        spend(self.timeout.unwrap_or(Duration::ZERO));
        io::Error::new(io::ErrorKind::TimedOut, "timed out")
    }

    fn respond(&self) -> VecDeque<(Duration, Vec<u8>)> {
        let text = String::from_utf8_lossy(&self.request);
        let mut lines = text.lines();
        let target = lines.next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
        let host = lines.find_map(|l| l.strip_prefix("Host: ")).unwrap_or_default();
        let url = format!("{host}{target}");
        let nth = {
            let mut asked = self.net.asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let count = asked.entry(url.clone()).or_insert(0);
            *count += 1;
            *count - 1
        };
        let (status, body) = match self.net.http.get(&url) {
            Some((statuses, body)) => (statuses.get(nth).or(statuses.last()).copied().unwrap_or(200), body.clone()),
            None => (404, format!("no `http {url} …` line in this fixture's net file")),
        };
        // a slow answer promises more than it ever sends
        let length = body.len() + if self.tcp.behaviour == Behaviour::Slow { 1000 } else { 0 };
        let head = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: text/plain\r\nContent-Length: {length}\r\n\r\n");
        if body.is_empty() {
            // nothing follows the head: the whole response time passes before it
            return VecDeque::from([(self.tcp.first + self.tcp.rest, head.into_bytes())]);
        }
        VecDeque::from([(self.tcp.first, head.into_bytes()), (self.tcp.rest, body.into_bytes())])
    }
}

impl Write for NetConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.request.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for NetConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.current.read(buf)?;
            if n > 0 || buf.is_empty() {
                return Ok(n);
            }
            if self.queue.is_none() {
                match self.tcp.behaviour {
                    Behaviour::Reset => {
                        return Err(io::Error::new(io::ErrorKind::ConnectionReset, "Connection reset by peer"));
                    }
                    Behaviour::Silent => return Err(self.time_out()),
                    _ => self.queue = Some(self.respond()),
                }
            }
            match self.queue.as_mut().and_then(VecDeque::pop_front) {
                Some((took, part)) => {
                    spend(took);
                    self.current = io::Cursor::new(part);
                }
                None if self.tcp.behaviour == Behaviour::Slow => return Err(self.time_out()),
                None => return Ok(0),
            }
        }
    }
}

impl Conn for NetConn {
    fn set_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.timeout = timeout;
        Ok(())
    }

    fn fake_tls(&mut self) -> Option<io::Result<()>> {
        Some(match &self.tcp.behaviour {
            Behaviour::TlsTimeout => Err(self.time_out()),
            Behaviour::TlsError(message) => Err(io::Error::new(io::ErrorKind::InvalidData, message.clone())),
            _ => {
                spend(self.tcp.tls);
                Ok(())
            }
        })
    }
}
