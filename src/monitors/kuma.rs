//! Uptime Kuma's socket API — the one its web UI uses; Kuma has no REST API for changing monitors.
//! Event names and payloads follow Kuma's `server/server.js` (1.23 and 2.x) and are exercised against
//! real Kuma servers in the CI integration job.

use std::sync::Arc;

use serde_json::{Value, json};

use super::inventory::{DockerHost, Inventory, KumaMonitor};
use super::socket::Session;
use crate::host::Host;

#[derive(Debug)]
pub struct Kuma {
    session: Session,
    /// Kuma's version (`1.23.17`, `2.0.2`), from its `info` event.
    pub version: String,
}

/// `{ok: true, …}` or the server's message.
fn ok(what: &str, answer: Value) -> Result<Value, String> {
    if answer["ok"].as_bool() == Some(true) {
        Ok(answer)
    } else {
        let message = answer["msg"].as_str().map_or_else(|| answer.to_string(), str::to_string);
        Err(format!("{what}: {message}"))
    }
}

impl Kuma {
    /// Connect without logging in.
    pub fn connect(host: &Arc<dyn Host>, url: &str) -> Result<Self, String> {
        let session = Session::connect(host, url).map_err(|e| format!("cannot reach Uptime Kuma at {url}: {e}"))?;
        Ok(Self { session, version: String::new() })
    }

    /// Whether this Kuma has no user yet (a fresh install waiting for its setup page).
    pub fn needs_setup(&mut self) -> Result<bool, String> {
        Ok(self.session.emit("needSetup", &[])?.as_bool().unwrap_or(false))
    }

    /// Create the first user of a fresh Kuma.
    pub fn setup(&mut self, username: &str, password: &str) -> Result<(), String> {
        ok("setup", self.session.emit("setup", &[json!(username), json!(password)])?).map(|_| ())
    }

    /// Log in and wait for the lists Kuma sends afterwards.
    pub fn login(&mut self, username: &str, password: &str) -> Result<(), String> {
        // Kuma also sends `info` before the login, without its version: only the one after counts
        self.session.forget("info");
        let answer =
            self.session.emit("login", &[json!({ "username": username, "password": password, "token": "" })])?;
        if answer["tokenRequired"].as_bool() == Some(true) {
            return Err("this Uptime Kuma user has two-factor authentication, which moi cannot answer: give moi a user of its own without 2FA".to_string());
        }
        ok("login failed", answer)?;
        self.session.event("monitorList")?;
        self.version = self.session.event("info")?["version"].as_str().unwrap_or_default().to_string();
        Ok(())
    }

    fn major(&self) -> u32 {
        self.version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(1)
    }

    /// What Kuma has, as it says itself (the same shape the read-only inventory has).
    pub fn inventory(&mut self) -> Result<Inventory, String> {
        let list = self.session.event("monitorList")?;
        let notifications: Vec<(i64, String)> = self
            .session
            .event("notificationList")?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| Some((n["id"].as_i64()?, n["name"].as_str()?.to_string())))
            .collect();
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
        let mut monitors: Vec<KumaMonitor> = list
            .as_object()
            .into_iter()
            .flatten()
            .map(|(_, m)| KumaMonitor {
                id: m["id"].as_i64().unwrap_or_default(),
                name: text(&m["name"]).unwrap_or_default(),
                kind: text(&m["type"]).unwrap_or_default(),
                // 1.23 says true/false, older builds 1/0
                active: m["active"].as_bool().unwrap_or_else(|| m["active"].as_i64() != Some(0)),
                interval: m["interval"].as_i64(),
                retries: m["maxretries"].as_i64(),
                push_token: text(&m["pushToken"]),
                docker_container: text(&m["docker_container"]),
                docker_host: m["docker_host"].as_i64().filter(|id| *id != 0),
                url: text(&m["url"]),
                parent: m["parent"].as_i64().filter(|id| *id != 0),
                tags: m["tags"].as_array().into_iter().flatten().filter_map(|t| text(&t["name"])).collect(),
                notifications: m["notificationIDList"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(_, on)| on.as_bool() == Some(true))
                    .filter_map(|(id, _)| {
                        notifications.iter().find(|(nid, _)| nid.to_string() == *id).map(|(_, name)| name.clone())
                    })
                    .collect(),
                last_status: None,
            })
            .collect();
        monitors.sort_by_key(|m| m.id);
        // sent after every login since Kuma 1.18; an older one simply has none
        let docker_hosts = self
            .session
            .event_within("dockerHostList", 2)
            .unwrap_or_default()
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| {
                Some(DockerHost {
                    id: h["id"].as_i64()?,
                    name: text(&h["name"]).unwrap_or_default(),
                    daemon: text(&h["dockerDaemon"]).unwrap_or_default(),
                })
            })
            .collect();
        let tags = self.tags()?;
        Ok(Inventory { db: String::new(), monitors, notifications, tags, docker_hosts })
    }

    pub fn tags(&mut self) -> Result<Vec<(i64, String)>, String> {
        let answer = ok("cannot list tags", self.session.emit("getTags", &[])?)?;
        Ok(answer["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| Some((t["id"].as_i64()?, t["name"].as_str()?.to_string())))
            .collect())
    }

    pub fn add_tag(&mut self, name: &str, color: &str) -> Result<i64, String> {
        let answer =
            ok("cannot create the tag", self.session.emit("addTag", &[json!({ "name": name, "color": color })])?)?;
        answer["tag"]["id"].as_i64().ok_or_else(|| "Kuma created the tag without telling its id".to_string())
    }

    pub fn tag_monitor(&mut self, tag: i64, monitor: i64) -> Result<(), String> {
        ok("cannot tag the monitor", self.session.emit("addMonitorTag", &[json!(tag), json!(monitor), json!("")])?)
            .map(|_| ())
    }

    /// Register a Docker daemon Kuma can ask about containers; returns its id.
    pub fn add_docker_host(&mut self, name: &str, daemon: &str) -> Result<i64, String> {
        let host = json!({ "name": name, "dockerType": if daemon.starts_with('/') { "socket" } else { "tcp" }, "dockerDaemon": daemon });
        let answer = ok("cannot add the docker host", self.session.emit("addDockerHost", &[host, Value::Null])?)?;
        answer["id"].as_i64().ok_or_else(|| "Kuma added the docker host without telling its id".to_string())
    }

    /// Create a monitor from the fields that matter; returns its id. Everything else gets the value
    /// Kuma's own form would send.
    pub fn add_monitor(&mut self, fields: Value) -> Result<i64, String> {
        let mut monitor = json!({
            "retryInterval": 60,
            "resendInterval": 0,
            "maxretries": 1,
            "upsideDown": false,
            "notificationIDList": {},
            "accepted_statuscodes": ["200-299"],
            "kafkaProducerBrokers": [],
            "kafkaProducerSaslOptions": { "mechanism": "None" },
        });
        if self.major() >= 2 {
            // columns that only exist in 2.x (an unknown key is an SQL error in 1.x)
            monitor["conditions"] = json!([]);
            monitor["rabbitmqNodes"] = json!([]);
        }
        for (key, value) in fields.as_object().into_iter().flatten() {
            monitor[key] = value.clone();
        }
        let name = monitor["name"].as_str().unwrap_or_default().to_string();
        let answer = ok(&format!("cannot create monitor \"{name}\""), self.session.emit("add", &[monitor])?)?;
        answer["monitorID"].as_i64().ok_or_else(|| "Kuma created the monitor without telling its id".to_string())
    }

    /// A monitor exactly as Kuma's edit form gets it.
    pub fn monitor(&mut self, id: i64) -> Result<Value, String> {
        Ok(ok("cannot read the monitor", self.session.emit("getMonitor", &[json!(id)])?)?["monitor"].clone())
    }

    /// Save a monitor read with [`Kuma::monitor`] and changed.
    pub fn edit_monitor(&mut self, monitor: &Value) -> Result<(), String> {
        ok("cannot change the monitor", self.session.emit("editMonitor", std::slice::from_ref(monitor))?).map(|_| ())
    }

    pub fn pause(&mut self, id: i64) -> Result<(), String> {
        ok("cannot pause the monitor", self.session.emit("pauseMonitor", &[json!(id)])?).map(|_| ())
    }

    pub fn resume(&mut self, id: i64) -> Result<(), String> {
        ok("cannot resume the monitor", self.session.emit("resumeMonitor", &[json!(id)])?).map(|_| ())
    }

    pub fn delete(&mut self, id: i64) -> Result<(), String> {
        ok("cannot delete the monitor", self.session.emit("deleteMonitor", &[json!(id)])?).map(|_| ())
    }
}
