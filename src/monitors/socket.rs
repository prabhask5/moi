//! A socket.io (v4) client over Engine.IO's HTTP long-polling transport — what Uptime Kuma's web UI
//! speaks, minus the WebSocket upgrade. Polling is plain request/response, so it runs on the blocking
//! HTTP client moi already has, needs no extra dependency, and is trivially answered by a test.
//!
//! One session, used from one thread: `emit` sends an event and polls until its acknowledgement
//! arrives; everything else the server sends meanwhile is kept by event name (latest payload wins).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::host::Host;
use crate::net::Client;

/// Engine.IO separates the packets of one payload with this.
const SEPARATOR: char = '\u{1e}';
/// The server answers a poll at the latest when its ping is due (25 s by default).
const POLL_TIMEOUT: Duration = Duration::from_secs(45);
/// Polls without the awaited packet before giving up (each can last a ping interval).
const MAX_POLLS: usize = 8;

#[derive(Debug)]
pub struct Session {
    client: Client,
    host: Arc<dyn Host>,
    /// `http://kuma/socket.io/?EIO=4&transport=polling`
    base: String,
    sid: String,
    next_ack: u64,
    /// Acknowledgements received and not yet claimed.
    acks: BTreeMap<u64, Value>,
    /// The latest arguments of every event the server emitted.
    events: BTreeMap<String, Value>,
}

fn problem(what: impl Into<String>) -> String {
    what.into()
}

impl Session {
    /// Open a session with the socket.io server at `url` (the site's base URL).
    pub fn connect(host: &Arc<dyn Host>, url: &str) -> Result<Self, String> {
        let base = format!("{}/socket.io/?EIO=4&transport=polling", url.trim_end_matches('/'));
        let mut session = Self {
            client: Client::new(Arc::clone(host)).with_timeout(POLL_TIMEOUT),
            host: Arc::clone(host),
            base,
            sid: String::new(),
            next_ack: 0,
            acks: BTreeMap::new(),
            events: BTreeMap::new(),
        };
        // Engine.IO handshake: the first poll returns the session id
        let opened = session.get()?;
        let open = opened.split(SEPARATOR).find_map(|p| p.strip_prefix('0')).ok_or_else(|| {
            problem(format!("not a socket.io server: {}", opened.chars().take(80).collect::<String>()))
        })?;
        let open: Value = serde_json::from_str(open).map_err(|e| problem(format!("bad handshake: {e}")))?;
        session.sid = open["sid"].as_str().ok_or_else(|| problem("handshake without a session id"))?.to_string();
        // socket.io: join the default namespace, and wait until the server says we are in
        session.post("40")?;
        for _ in 0..MAX_POLLS {
            let payload = session.get()?;
            let joined = payload.split(SEPARATOR).any(|p| p.starts_with("40"));
            session.take(&payload)?;
            if joined {
                return Ok(session);
            }
        }
        Err(problem("the server did not accept the connection"))
    }

    fn url(&self) -> String {
        // `t` only defeats caches; it need not be unpredictable
        let t: String = self.host.random_bytes(6).unwrap_or_default().iter().map(|b| format!("{b:02x}")).collect();
        let sid = if self.sid.is_empty() { String::new() } else { format!("&sid={}", self.sid) };
        format!("{}&t={t}{sid}", self.base)
    }

    fn get(&self) -> Result<String, String> {
        let response = self.client.send("GET", &self.url(), &[], None).map_err(|e| problem(e.to_string()))?;
        if !response.is_success() {
            return Err(problem(format!(
                "HTTP {} from the socket.io endpoint: {}",
                response.status,
                response.text().chars().take(120).collect::<String>()
            )));
        }
        Ok(response.text())
    }

    fn post(&self, packet: &str) -> Result<(), String> {
        let headers = [("Content-Type".to_string(), "text/plain;charset=UTF-8".to_string())];
        let response = self
            .client
            .send("POST", &self.url(), &headers, Some(packet.as_bytes()))
            .map_err(|e| problem(e.to_string()))?;
        if !response.is_success() {
            return Err(problem(format!(
                "HTTP {} from the socket.io endpoint: {}",
                response.status,
                response.text().chars().take(120).collect::<String>()
            )));
        }
        Ok(())
    }

    /// Take in one polled payload: answer pings, file events and acknowledgements.
    fn take(&mut self, payload: &str) -> Result<(), String> {
        for packet in payload.split(SEPARATOR) {
            match packet.as_bytes().first() {
                // ping → pong, or the server drops the session
                Some(b'2') => self.post("3")?,
                Some(b'1') => return Err(problem("the server closed the session")),
                Some(b'4') => self.message(&packet[1..])?,
                _ => {}
            }
        }
        Ok(())
    }

    /// One socket.io packet: `2["event", args…]`, `3<id>[args…]`, `4{"message": …}`.
    fn message(&mut self, packet: &str) -> Result<(), String> {
        let kind = packet.as_bytes().first().copied();
        let rest = packet.get(1..).unwrap_or_default();
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let (id, json) = rest.split_at(digits);
        match kind {
            Some(b'2') => {
                if let Ok(Value::Array(mut items)) = serde_json::from_str::<Value>(json)
                    && !items.is_empty()
                    && let Value::String(event) = items.remove(0)
                {
                    self.events.insert(event, Value::Array(items));
                }
            }
            Some(b'3') => {
                if let (Ok(id), Ok(args)) = (id.parse::<u64>(), serde_json::from_str::<Value>(json)) {
                    self.acks.insert(id, args);
                }
            }
            Some(b'4') => return Err(problem(format!("the server refused the connection: {json}"))),
            Some(b'1') => return Err(problem("the server disconnected")),
            _ => {}
        }
        Ok(())
    }

    /// Emit an event and wait for its acknowledgement; returns the acknowledgement's first argument.
    pub fn emit(&mut self, event: &str, args: &[Value]) -> Result<Value, String> {
        let id = self.next_ack;
        self.next_ack += 1;
        let mut items = vec![Value::String(event.to_string())];
        items.extend_from_slice(args);
        self.post(&format!("42{id}{}", Value::Array(items)))?;
        for _ in 0..MAX_POLLS {
            if let Some(ack) = self.acks.remove(&id) {
                return Ok(ack.get(0).cloned().unwrap_or(Value::Null));
            }
            let payload = self.get()?;
            self.take(&payload)?;
        }
        self.acks
            .remove(&id)
            .map(|ack| ack.get(0).cloned().unwrap_or(Value::Null))
            .ok_or_else(|| problem(format!("no answer to `{event}`")))
    }

    /// The first argument of the latest `event` the server emitted, polling until one has arrived.
    pub fn event(&mut self, event: &str) -> Result<Value, String> {
        self.event_within(event, MAX_POLLS)
    }

    /// Like [`Session::event`], giving up after this many polls (for events a version may not send).
    pub fn event_within(&mut self, event: &str, polls: usize) -> Result<Value, String> {
        for _ in 0..polls {
            if let Some(args) = self.events.get(event) {
                return Ok(args.get(0).cloned().unwrap_or(Value::Null));
            }
            let payload = self.get()?;
            self.take(&payload)?;
        }
        Err(problem(format!("the server never sent `{event}`")))
    }

    /// The latest `event` if the server has sent one already (no waiting).
    pub fn seen(&self, event: &str) -> Option<Value> {
        self.events.get(event).map(|args| args.get(0).cloned().unwrap_or(Value::Null))
    }

    /// Forget an event, so that [`Session::event`] waits for the next one.
    pub fn forget(&mut self, event: &str) {
        self.events.remove(event);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // tell the server we are leaving, so it does not keep the session until its ping times out
        if !self.sid.is_empty() {
            let _ = self.post("1");
        }
    }
}
