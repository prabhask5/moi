//! A stand-in for Uptime Kuma: its socket API over Engine.IO long-polling and its push endpoint, kept in
//! memory. It behaves the way Kuma's `server/server.js` does for the events moi uses — which is this
//! file's reading of that code; the integration job (`tests/monitors_sync.rs`, ignored tests) is what
//! checks moi against real Kuma servers.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use cestmoi::host::{FixtureHost, HttpHandler};
use cestmoi::monitors::Inventory;
use serde_json::{Value, json};

#[derive(Default)]
pub struct State {
    pub version: String,
    pub users: Vec<(String, String)>,
    /// A user with two-factor authentication.
    pub two_factor: bool,
    pub monitors: BTreeMap<i64, Value>,
    pub notifications: Vec<Value>,
    pub tags: Vec<Value>,
    /// (tag id, monitor id)
    pub monitor_tags: Vec<(i64, i64)>,
    pub docker_hosts: Vec<Value>,
    next_id: i64,
    /// session id → (logged in, packets waiting to be polled)
    sessions: BTreeMap<String, (bool, Vec<String>)>,
    /// Every event received, as `name args…`.
    pub events: Vec<String>,
    /// Every push received, as `token status msg`.
    pub pushes: Vec<String>,
    /// `add` fails for monitors with this name.
    pub refuse_name: Option<String>,
    pub pings_sent: usize,
    pub pongs: usize,
}

#[derive(Clone)]
pub struct FakeKuma(pub Arc<Mutex<State>>);

fn http(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl FakeKuma {
    /// An empty Kuma of this version with one user.
    pub fn new(version: &str, username: &str, password: &str) -> Self {
        let state = State {
            version: version.to_string(),
            users: vec![(username.to_string(), password.to_string())],
            next_id: 100,
            ..State::default()
        };
        Self(Arc::new(Mutex::new(state)))
    }

    /// Fill it with what an inventory says Kuma has.
    pub fn seed(&self, inventory: &Inventory) {
        let mut state = self.0.lock().unwrap();
        for (id, name) in &inventory.notifications {
            state.notifications.push(json!({ "id": id, "name": name, "active": true, "isDefault": false }));
        }
        for (id, name) in &inventory.tags {
            state.tags.push(json!({ "id": id, "name": name, "color": "#000000" }));
        }
        for m in &inventory.monitors {
            let notifications: serde_json::Map<String, Value> = inventory
                .notifications
                .iter()
                .filter(|(_, n)| m.notifications.contains(n))
                .map(|(id, _)| (id.to_string(), json!(true)))
                .collect();
            state.monitors.insert(
                m.id,
                json!({
                    "id": m.id, "name": m.name, "type": m.kind, "active": m.active, "interval": m.interval, "maxretries": m.retries.unwrap_or(0),
                    "pushToken": m.push_token, "docker_container": m.docker_container, "docker_host": m.docker_host, "url": m.url, "parent": m.parent,
                    "notificationIDList": notifications, "accepted_statuscodes": ["200-299"], "retryInterval": 60,
                }),
            );
            for tag in &m.tags {
                if let Some((id, _)) = inventory.tags.iter().find(|(_, name)| name == tag) {
                    state.monitor_tags.push((*id, m.id));
                }
            }
        }
    }

    /// Be the network of this fixture host.
    pub fn serve(&self, fixture: &FixtureHost) {
        let kuma = self.clone();
        let handler: HttpHandler = Arc::new(move |_authority, raw| kuma.answer(raw));
        fixture.set_http(handler);
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    /// Events that change something, in order.
    pub fn changes(&self) -> Vec<String> {
        let reads = ["login", "getTags", "getMonitor", "needSetup"];
        self.state().events.iter().filter(|e| !reads.contains(&e.split(' ').next().unwrap())).cloned().collect()
    }

    pub fn monitor_named(&self, name: &str) -> Option<Value> {
        self.state().monitors.values().find(|m| m["name"] == name).cloned()
    }

    pub fn tags_of(&self, monitor: i64) -> Vec<String> {
        let state = self.state();
        state
            .monitor_tags
            .iter()
            .filter(|(_, m)| *m == monitor)
            .filter_map(|(t, _)| state.tags.iter().find(|tag| tag["id"] == *t)?["name"].as_str().map(str::to_string))
            .collect()
    }

    fn answer(&self, raw: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(raw);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let mut first = head.lines().next().unwrap_or_default().split_whitespace();
        let (method, target) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let param = |name: &str| query.split('&').find_map(|pair| pair.strip_prefix(&format!("{name}="))).map(decode);
        let mut state = self.0.lock().unwrap();
        if let Some(token) = path.strip_prefix("/api/push/") {
            let known = state.monitors.values().any(|m| m["pushToken"] == token && m["active"] != false);
            if !known {
                return http("404 Not Found", r#"{"ok":false,"msg":"Monitor not found or not active."}"#);
            }
            state.pushes.push(format!(
                "{token} {} {}",
                param("status").unwrap_or_default(),
                param("msg").unwrap_or_default()
            ));
            return http("200 OK", r#"{"ok":true}"#);
        }
        if path != "/socket.io/" {
            return http("404 Not Found", "not found");
        }
        let Some(sid) = param("sid") else {
            let sid = format!("sid{}", state.sessions.len() + 1);
            state.sessions.insert(sid.clone(), (false, Vec::new()));
            return http(
                "200 OK",
                &format!(
                    r#"0{{"sid":"{sid}","upgrades":["websocket"],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}}"#
                ),
            );
        };
        if !state.sessions.contains_key(&sid) {
            return http("400 Bad Request", r#"{"code":1,"message":"Session ID unknown"}"#);
        }
        if method == "GET" {
            let waiting = std::mem::take(&mut state.sessions.get_mut(&sid).unwrap().1);
            if waiting.is_empty() {
                // nothing to say: the real server would hold the poll until its ping is due
                state.pings_sent += 1;
                return http("200 OK", "2");
            }
            return http("200 OK", &waiting.join("\u{1e}"));
        }
        for packet in body.split('\u{1e}') {
            if packet == "40" {
                let info = json!(["info", { "primaryBaseURL": null, "serverTimezone": "UTC" }]);
                state
                    .sessions
                    .get_mut(&sid)
                    .unwrap()
                    .1
                    .extend([format!(r#"40{{"sid":"{sid}-io"}}"#), format!("42{info}")]);
            } else if packet == "3" {
                state.pongs += 1;
            } else if packet == "1" {
                state.sessions.remove(&sid);
            } else if let Some(rest) = packet.strip_prefix("42") {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                let (id, json_text) = rest.split_at(digits);
                let mut items: Vec<Value> = serde_json::from_str(json_text).unwrap();
                let event = items.remove(0).as_str().unwrap().to_string();
                let (answer, after) = state.handle(&sid, &event, &items);
                let Some(session) = state.sessions.get_mut(&sid) else { continue };
                session.1.push(format!("43{id}[{answer}]"));
                session.1.extend(after);
            }
        }
        http("200 OK", "ok")
    }
}

impl State {
    fn monitor_list(&self) -> Value {
        let list: serde_json::Map<String, Value> = self
            .monitors
            .iter()
            .map(|(id, m)| {
                let mut m = m.clone();
                let tags: Vec<Value> = self
                    .monitor_tags
                    .iter()
                    .filter(|(_, monitor)| monitor == id)
                    .filter_map(|(tag, _)| self.tags.iter().find(|t| t["id"] == *tag))
                    .map(|t| json!({ "tag_id": t["id"], "monitor_id": id, "value": "", "name": t["name"], "color": t["color"] }))
                    .collect();
                m["tags"] = json!(tags);
                (id.to_string(), m)
            })
            .collect();
        Value::Object(list)
    }

    fn next(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    /// (acknowledgement, events emitted afterwards)
    fn handle(&mut self, sid: &str, event: &str, args: &[Value]) -> (Value, Vec<String>) {
        // passwords never go into the log of events
        let shown: Vec<String> =
            if event == "login" || event == "setup" { Vec::new() } else { args.iter().map(Value::to_string).collect() };
        self.events.push(format!("{event} {}", shown.join(" ")).trim_end().to_string());
        let emit = |name: &str, value: Value| format!("42{}", json!([name, value]));
        match event {
            "needSetup" => return (json!(self.users.is_empty()), Vec::new()),
            "setup" => {
                if !self.users.is_empty() {
                    return (
                        json!({ "ok": false, "msg": "Uptime Kuma has been initialized. If you want to run setup again, please delete the database." }),
                        Vec::new(),
                    );
                }
                self.users.push((args[0].as_str().unwrap().to_string(), args[1].as_str().unwrap().to_string()));
                return (json!({ "ok": true, "msg": "Added Successfully." }), Vec::new());
            }
            "login" => {
                let (user, password) = (
                    args[0]["username"].as_str().unwrap_or_default(),
                    args[0]["password"].as_str().unwrap_or_default(),
                );
                if !self.users.iter().any(|(u, p)| u == user && p == password) {
                    return (json!({ "ok": false, "msg": "Incorrect username or password." }), Vec::new());
                }
                if self.two_factor {
                    return (json!({ "tokenRequired": true }), Vec::new());
                }
                self.sessions.get_mut(sid).unwrap().0 = true;
                let after = vec![
                    emit("monitorList", self.monitor_list()),
                    emit(
                        "info",
                        json!({ "version": self.version, "latestVersion": self.version, "primaryBaseURL": null, "serverTimezone": "UTC" }),
                    ),
                    emit("notificationList", json!(self.notifications)),
                    emit("dockerHostList", json!(self.docker_hosts)),
                    emit("heartbeatList", json!([1, [], true])),
                ];
                return (json!({ "ok": true, "token": "FAKE.jwt.token" }), after);
            }
            _ => {}
        }
        if !self.sessions.get(sid).is_some_and(|s| s.0) {
            return (json!({ "ok": false, "msg": "You are not logged in." }), Vec::new());
        }
        let id_arg = args.first().and_then(Value::as_i64).unwrap_or_default();
        let answer = match event {
            "getTags" => json!({ "ok": true, "tags": self.tags }),
            "addTag" => {
                let tag = json!({ "id": self.next(), "name": args[0]["name"], "color": args[0]["color"] });
                self.tags.push(tag.clone());
                json!({ "ok": true, "tag": tag })
            }
            "addMonitorTag" => {
                self.monitor_tags.push((args[0].as_i64().unwrap(), args[1].as_i64().unwrap()));
                json!({ "ok": true, "msg": "Added Successfully." })
            }
            "addDockerHost" => {
                let id = self.next();
                self.docker_hosts.push(json!({ "id": id, "name": args[0]["name"], "dockerDaemon": args[0]["dockerDaemon"], "dockerType": args[0]["dockerType"] }));
                json!({ "ok": true, "msg": "Saved.", "id": id })
            }
            "add" => {
                let mut monitor = args[0].clone();
                // what the real server would choke on
                let two = self.version.starts_with('2');
                let unknown = monitor
                    .as_object()
                    .unwrap()
                    .keys()
                    .find(|k| !two && ["conditions", "rabbitmqNodes"].contains(&k.as_str()))
                    .cloned();
                if let Some(column) = unknown {
                    json!({ "ok": false, "msg": format!("table monitor has no column named {column}") })
                } else if two && !monitor["conditions"].is_array() {
                    json!({ "ok": false, "msg": "SQLITE_CONSTRAINT: NOT NULL constraint failed: monitor.conditions" })
                } else if !monitor["accepted_statuscodes"].is_array()
                    || monitor["name"].as_str().is_none_or(str::is_empty)
                {
                    json!({ "ok": false, "msg": "Invalid monitor" })
                } else if self.refuse_name.as_deref() == monitor["name"].as_str() {
                    json!({ "ok": false, "msg": "database is locked" })
                } else {
                    let id = self.next();
                    monitor["id"] = json!(id);
                    monitor["active"] = json!(true);
                    self.monitors.insert(id, monitor);
                    json!({ "ok": true, "msg": "Added Successfully.", "monitorID": id })
                }
            }
            "getMonitor" => match self.monitors.get(&id_arg) {
                Some(m) => json!({ "ok": true, "monitor": m }),
                None => json!({ "ok": false, "msg": "Monitor not found" }),
            },
            "editMonitor" => {
                let id = args[0]["id"].as_i64().unwrap_or_default();
                match self.monitors.get_mut(&id) {
                    Some(monitor) => {
                        *monitor = args[0].clone();
                        json!({ "ok": true, "msg": "Saved.", "monitorID": id })
                    }
                    None => json!({ "ok": false, "msg": "Monitor not found" }),
                }
            }
            "pauseMonitor" | "resumeMonitor" => match self.monitors.get_mut(&id_arg) {
                Some(m) => {
                    m["active"] = json!(event == "resumeMonitor");
                    json!({ "ok": true, "msg": "Done" })
                }
                None => json!({ "ok": false, "msg": "Monitor not found" }),
            },
            "deleteMonitor" => {
                if self.monitors.remove(&id_arg).is_some() {
                    self.monitor_tags.retain(|(_, m)| *m != id_arg);
                    json!({ "ok": true, "msg": "Deleted Successfully." })
                } else {
                    json!({ "ok": false, "msg": "Monitor not found" })
                }
            }
            other => json!({ "ok": false, "msg": format!("unknown event {other}") }),
        };
        (answer, Vec::new())
    }
}
