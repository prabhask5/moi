//! A fake Cloudflare API for `FixtureHost`: tunnels with their remote configuration, zones and DNS
//! records, and a token with or without each of the two permissions `moi ssh` needs. Everything a
//! test wants to assert afterwards is in its state: what the tunnel's ingress is now, which records
//! exist, every request in order.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use cestmoi::host::{FixtureHost, HttpHandler};
use serde_json::{Value, json};

pub const ACCOUNT: &str = "FAKEaccount0000000000000000000001";
pub const TUNNEL: &str = "FAKEtunn-0000-4000-8000-000000000001";
pub const API_TOKEN: &str = "FAKEcloudflareapitoken0000000000000000";
pub const ZONE: &str = "FAKEzone000000000000000000000001";

#[derive(Debug)]
pub struct State {
    /// The token that is accepted.
    pub token: String,
    /// Account → Cloudflare Tunnel → Edit.
    pub may_edit_tunnels: bool,
    /// Zone → DNS → Edit.
    pub may_edit_dns: bool,
    pub tunnel_exists: bool,
    /// The tunnel's remote configuration (`ingress` among it).
    pub config: Value,
    /// Zone names this account has (all under the id [`ZONE`]).
    pub zones: Vec<String>,
    pub records: Vec<Value>,
    /// `METHOD /path?query`, in order.
    pub requests: Vec<String>,
    next_record: u32,
}

#[derive(Debug, Clone)]
pub struct FakeCloudflare(Arc<Mutex<State>>);

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= chunk.len() { char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]) } else { '=' });
        }
    }
    out
}

/// A connector token for the fake tunnel, made the way Cloudflare makes them: base64 of `{a, t, s}`.
/// Built at run time so that no token-shaped string is committed.
pub fn tunnel_token() -> String {
    base64(json!({ "a": ACCOUNT, "t": TUNNEL, "s": "FAKEtunnelsecret" }).to_string().as_bytes())
}

/// A token of some other tunnel.
pub fn other_tunnel_token() -> String {
    base64(json!({ "a": ACCOUNT, "t": "FAKEtunn-other", "s": "FAKEtunnelsecret" }).to_string().as_bytes())
}

fn respond(status: u16, body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        .into_bytes()
}

fn ok(result: Value) -> Vec<u8> {
    respond(200, &json!({ "success": true, "errors": [], "messages": [], "result": result }))
}

fn error(status: u16, code: u32, message: &str) -> Vec<u8> {
    respond(status, &json!({ "success": false, "errors": [{ "code": code, "message": message }], "result": null }))
}

impl FakeCloudflare {
    /// An account with one tunnel (catch-all rule only), the zone `example.com` and no records.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(State {
            token: API_TOKEN.to_string(),
            may_edit_tunnels: true,
            may_edit_dns: true,
            tunnel_exists: true,
            config: json!({ "ingress": [{ "service": "http_status:404" }], "warp-routing": { "enabled": false } }),
            zones: vec!["example.com".to_string()],
            records: Vec::new(),
            requests: Vec::new(),
            next_record: 1,
        })))
    }

    /// Be `api.cloudflare.com` for this fixture host.
    pub fn serve(&self, fixture: &FixtureHost) {
        let fake = self.clone();
        let handler: HttpHandler = Arc::new(move |_authority, raw| fake.answer(raw));
        fixture.set_http_at("api.cloudflare.com:443", handler);
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    pub fn ingress(&self) -> Vec<Value> {
        self.state().config["ingress"].as_array().cloned().unwrap_or_default()
    }

    pub fn set_ingress(&self, rules: Value) {
        self.state().config["ingress"] = rules;
    }

    pub fn add_record(&self, kind: &str, name: &str, content: &str) {
        let mut state = self.state();
        let id = format!("FAKErecord{:04}", state.next_record);
        state.next_record += 1;
        state.records.push(json!({ "id": id, "type": kind, "name": name, "content": content, "proxied": true }));
    }

    /// Requests that change something.
    pub fn changes(&self) -> Vec<String> {
        self.state().requests.iter().filter(|r| !r.starts_with("GET ")).cloned().collect()
    }

    fn answer(&self, raw: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(raw).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let mut first = head.lines().next().unwrap_or_default().split_whitespace();
        let (method, target) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
        let bearer = head
            .lines()
            .find_map(|l| l.strip_prefix("Authorization: Bearer ").or_else(|| l.strip_prefix("authorization: Bearer ")))
            .unwrap_or_default()
            .trim();
        let mut state = self.state();
        state.requests.push(format!("{method} {target}"));
        if bearer != state.token {
            return error(401, 1000, "Invalid API Token");
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let segments: Vec<&str> = path.trim_start_matches("/client/v4/").split('/').collect();
        let wanted = query.split('&').find_map(|pair| pair.strip_prefix("name=")).unwrap_or_default().to_string();
        match (method, segments.as_slice()) {
            (_, ["accounts", _, "cfd_tunnel", ..]) if !state.may_edit_tunnels => {
                error(403, 10000, "Authentication error")
            }
            (_, ["accounts", account, "cfd_tunnel", tunnel, ..])
                if *account != ACCOUNT || *tunnel != TUNNEL || !state.tunnel_exists =>
            {
                error(404, 1003, "Tunnel not found")
            }
            ("GET", ["accounts", _, "cfd_tunnel", tunnel]) => {
                ok(json!({ "id": tunnel, "name": "fake", "status": "healthy" }))
            }
            ("GET", ["accounts", _, "cfd_tunnel", tunnel, "configurations"]) => {
                ok(json!({ "tunnel_id": tunnel, "version": 1, "config": state.config }))
            }
            ("PUT", ["accounts", _, "cfd_tunnel", tunnel, "configurations"]) => {
                let sent: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                // what the real API insists on: a catch-all, and it comes last
                let rules = sent["config"]["ingress"].as_array().cloned().unwrap_or_default();
                if rules.last().is_none_or(|r| r.get("hostname").is_some())
                    || rules.iter().rev().skip(1).any(|r| r.get("hostname").is_none())
                {
                    return error(
                        400,
                        1056,
                        "The last ingress rule must match all URLs (i.e. it should not have a hostname or path filter)",
                    );
                }
                state.config = sent["config"].clone();
                ok(json!({ "tunnel_id": tunnel, "version": 2, "config": state.config }))
            }
            ("DELETE", ["accounts", _, "cfd_tunnel", _]) => panic!("moi tried to delete a tunnel"),
            (_, ["zones", ..]) if !state.may_edit_dns => error(403, 10000, "Authentication error"),
            ("GET", ["zones"]) => ok(Value::Array(
                state.zones.iter().filter(|z| **z == wanted).map(|z| json!({ "id": ZONE, "name": z })).collect(),
            )),
            ("GET", ["zones", _, "dns_records"]) => {
                ok(Value::Array(state.records.iter().filter(|r| r["name"] == wanted.as_str()).cloned().collect()))
            }
            ("POST", ["zones", _, "dns_records"]) => {
                let mut record: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                if state.records.iter().any(|r| r["name"] == record["name"]) {
                    return error(400, 81053, "An A, AAAA, or CNAME record with that host already exists.");
                }
                record["id"] = json!(format!("FAKErecord{:04}", state.next_record));
                state.next_record += 1;
                state.records.push(record.clone());
                ok(record)
            }
            ("DELETE", ["zones", _, "dns_records", id]) => {
                let before = state.records.len();
                state.records.retain(|r| r["id"] != *id);
                if state.records.len() == before {
                    error(404, 81044, "Record does not exist.")
                } else {
                    ok(json!({ "id": id }))
                }
            }
            _ => error(404, 7003, "No route for that URI"),
        }
    }
}

/// What a connected cloudflared answers on its metrics address.
pub fn serve_metrics(fixture: &FixtureHost, address: &str, connections: u64) {
    let handler: HttpHandler = Arc::new(move |_authority, _raw| {
        let status = if connections > 0 { 200 } else { 503 };
        let body =
            json!({ "status": status, "readyConnections": connections, "connectorId": "FAKEconnector" }).to_string();
        format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
            .into_bytes()
    });
    fixture.set_http_at(address, handler);
}

/// The public name answering as a tunnel with an SSH origin does: a WebSocket whose first message is
/// the server's banner.
pub fn serve_ssh_over_websocket(fixture: &FixtureHost, hostname: &str, banner: &'static str) {
    let handler: HttpHandler = Arc::new(move |_authority, raw| {
        let head = String::from_utf8_lossy(raw).to_lowercase();
        assert!(
            head.contains("upgrade: websocket")
                && head.contains("sec-websocket-key:")
                && head.contains("sec-websocket-version: 13"),
            "not a WebSocket handshake:\n{head}"
        );
        let mut out = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: unchecked\r\n\r\n".to_vec();
        out.extend([0x82, u8::try_from(banner.len()).unwrap()]);
        out.extend(banner.as_bytes());
        out
    });
    fixture.set_http_at(&format!("{hostname}:443"), handler);
}
