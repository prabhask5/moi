//! Docker Engine API over the unix socket: containers, networks, volumes, swarm services and nodes,
//! events, exec, logs. No `docker` CLI.
//!
//! Spec: docs/spec/02-snapshot.md § Docker. Payloads are returned as `serde_json::Value` exactly as the
//! engine sent them: the snapshot normalizer picks the stable fields, and nothing breaks when an engine
//! version adds or drops one. Requests use unversioned paths, i.e. the engine's own current API version.
//!
//! Sync (see `http`); every connection comes from `host::Host`, so tests replay fixture files.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;

use crate::host::Host;
use crate::net::http1 as http;

/// Where the engine listens unless told otherwise.
pub const DEFAULT_SOCKET: &str = "/var/run/docker.sock";

#[derive(Debug, Error)]
pub enum DockerError {
    /// No engine: socket missing, connection refused, permission denied.
    #[error("docker is not reachable at {socket}: {source}")]
    Unavailable {
        socket: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The engine answered with an error status.
    #[error("docker: {method} {path}: {status} {message}")]
    Api { method: &'static str, path: String, status: u16, message: String },
    /// The connection broke or the answer was not what the API promises.
    #[error("docker: {method} {path}: {message}")]
    Protocol { method: &'static str, path: String, message: String },
}

impl DockerError {
    /// True for "no such container/network/…" answers.
    pub fn is_not_found(&self) -> bool {
        matches!(self, DockerError::Api { status: 404, .. })
    }
}

type Result<T> = std::result::Result<T, DockerError>;

/// A handle on the engine. Cheap to clone; every call opens its own connection.
#[derive(Debug, Clone)]
pub struct Docker {
    host: Arc<dyn Host>,
    socket: PathBuf,
    timeout: Duration,
}

/// Engine version as reported by `/version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub version: String,
    pub api_version: String,
}

/// One engine event (`/events`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// `container`, `network`, `volume`, `service`, …
    pub kind: String,
    /// `start`, `die`, `create`, `destroy`, `update`, …
    pub action: String,
    /// Id of the object the event is about.
    pub id: String,
    /// Actor attributes: `name`, `image`, labels, …
    pub attributes: BTreeMap<String, String>,
    /// Unix seconds.
    pub time: i64,
}

impl Event {
    /// The object's name (containers, networks, volumes, services all set it).
    pub fn name(&self) -> Option<&str> {
        self.attributes.get("name").map(String::as_str)
    }
}

/// What a command run inside a container produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub exit_code: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    pub fn stdout_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }
}

/// Options for [`Docker::exec`].
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    pub user: Option<String>,
    pub env: Vec<String>,
    pub workdir: Option<String>,
}

/// Options for [`Docker::logs`].
#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    /// Only entries at or after this unix time.
    pub since: Option<i64>,
    /// Only the last N lines.
    pub tail: Option<u32>,
}

/// A live `/events` stream; ends when the engine closes it.
pub struct Events {
    lines: std::io::Lines<BufReader<http::Body>>,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Events")
    }
}

impl Iterator for Events {
    type Item = Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let protocol = |message: String| DockerError::Protocol { method: "GET", path: "/events".into(), message };
            let line = match self.lines.next()? {
                Ok(line) => line,
                Err(e) => return Some(Err(protocol(e.to_string()))),
            };
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => return Some(Err(protocol(format!("event is not JSON: {e}")))),
            };
            let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
            let attributes = v["Actor"]["Attributes"]
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), text(v))).collect())
                .unwrap_or_default();
            return Some(Ok(Event {
                kind: text(&v["Type"]),
                action: text(&v["Action"]),
                id: text(&v["Actor"]["ID"]),
                attributes,
                time: v["time"].as_i64().unwrap_or_default(),
            }));
        }
    }
}

impl Docker {
    /// The engine at the default socket.
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self::at(host, Path::new(DEFAULT_SOCKET))
    }

    /// The engine at another socket path.
    pub fn at(host: Arc<dyn Host>, socket: &Path) -> Self {
        Self { host, socket: socket.to_path_buf(), timeout: Duration::from_secs(30) }
    }

    /// How long a request may wait for the engine (default 30 s; event streams never time out).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn send(&self, method: &'static str, path: &str, body: Option<&Value>, upgrade: bool) -> Result<http::Response> {
        let mut conn = self
            .host
            .connect_unix(&self.socket)
            .map_err(|source| DockerError::Unavailable { socket: self.socket.clone(), source })?;
        let protocol =
            |e: std::io::Error| DockerError::Protocol { method, path: path.to_string(), message: e.to_string() };
        conn.set_timeout(Some(self.timeout)).map_err(protocol)?;
        let payload = body.map(|b| b.to_string().into_bytes());
        let json = [("Content-Type".to_string(), "application/json".to_string())];
        let request = http::Request {
            method,
            target: path,
            host: "docker",
            headers: if payload.is_some() { &json } else { &[] },
            body: payload.as_deref(),
            upgrade,
        };
        let response = http::request(conn, &request).map_err(protocol)?;
        if response.status >= 400 {
            let status = response.status;
            let raw = response.bytes().unwrap_or_default();
            let message = serde_json::from_slice::<Value>(&raw)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| String::from_utf8_lossy(&raw).trim().to_string());
            return Err(DockerError::Api { method, path: path.to_string(), status, message });
        }
        Ok(response)
    }

    fn get(&self, path: &str) -> Result<Value> {
        let raw = self.send("GET", path, None, false)?.bytes().map_err(|e| DockerError::Protocol {
            method: "GET",
            path: path.to_string(),
            message: e.to_string(),
        })?;
        serde_json::from_slice(&raw).map_err(|e| DockerError::Protocol {
            method: "GET",
            path: path.to_string(),
            message: format!("answer is not JSON: {e}"),
        })
    }

    fn list(&self, path: &str) -> Result<Vec<Value>> {
        match self.get(path)? {
            Value::Array(items) => Ok(items),
            Value::Null => Ok(Vec::new()),
            _ => {
                Err(DockerError::Protocol { method: "GET", path: path.to_string(), message: "expected a list".into() })
            }
        }
    }

    /// True if an engine answers on the socket.
    pub fn ping(&self) -> bool {
        self.send("GET", "/_ping", None, false).is_ok()
    }

    pub fn version(&self) -> Result<Version> {
        let v = self.get("/version")?;
        let text = |key: &str| v[key].as_str().unwrap_or_default().to_string();
        Ok(Version { version: text("Version"), api_version: text("ApiVersion") })
    }

    /// `/info`: `DockerRootDir`, `Swarm.LocalNodeState`, …
    pub fn info(&self) -> Result<Value> {
        self.get("/info")
    }

    /// True if this engine is an active swarm node.
    pub fn swarm_active(&self) -> Result<bool> {
        Ok(self.info()?["Swarm"]["LocalNodeState"] == "active")
    }

    /// Container summaries (`/containers/json`), stopped ones included when `all`.
    pub fn containers(&self, all: bool) -> Result<Vec<Value>> {
        self.list(if all { "/containers/json?all=true" } else { "/containers/json" })
    }

    /// Container summaries with `SizeRw` (bytes written inside the container) and `SizeRootFs`. The engine
    /// walks every container's layer for this: audits ask, snapshots do not.
    pub fn containers_with_sizes(&self) -> Result<Vec<Value>> {
        self.list("/containers/json?all=true&size=true")
    }

    /// Full description of one container, by id or name.
    pub fn container(&self, id: &str) -> Result<Value> {
        self.get(&format!("/containers/{}/json", http::encode(id)))
    }

    /// Full descriptions of every container (one list call, one inspect each). A container removed
    /// between the two is skipped.
    pub fn containers_inspect(&self) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for summary in self.containers(true)? {
            let Some(id) = summary["Id"].as_str() else { continue };
            match self.container(id) {
                Ok(c) => out.push(c),
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Networks with IPAM configuration.
    pub fn networks(&self) -> Result<Vec<Value>> {
        self.list("/networks")
    }

    /// Volumes, anonymous ones included.
    pub fn volumes(&self) -> Result<Vec<Value>> {
        match self.get("/volumes")? {
            Value::Object(mut o) => match o.remove("Volumes") {
                Some(Value::Array(items)) => Ok(items),
                _ => Ok(Vec::new()),
            },
            _ => Ok(Vec::new()),
        }
    }

    pub fn images(&self) -> Result<Vec<Value>> {
        self.list("/images/json")
    }

    /// Swarm services (an error on engines that are not swarm managers).
    pub fn services(&self) -> Result<Vec<Value>> {
        self.list("/services")
    }

    pub fn nodes(&self) -> Result<Vec<Value>> {
        self.list("/nodes")
    }

    /// Swarm secrets: metadata only, the engine never returns their content.
    pub fn secrets(&self) -> Result<Vec<Value>> {
        self.list("/secrets")
    }

    pub fn configs(&self) -> Result<Vec<Value>> {
        self.list("/configs")
    }

    /// Follow engine events. `filters` are the API's filters (`{"type": ["container"], "event": ["start"]}`);
    /// `since`/`until` are unix seconds (with `until` the stream ends by itself).
    pub fn events(
        &self,
        filters: &BTreeMap<String, Vec<String>>,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<Events> {
        let mut query = Vec::new();
        if !filters.is_empty() {
            query.push(format!("filters={}", http::encode(&json!(filters).to_string())));
        }
        if let Some(since) = since {
            query.push(format!("since={since}"));
        }
        if let Some(until) = until {
            query.push(format!("until={until}"));
        }
        let path = if query.is_empty() { "/events".to_string() } else { format!("/events?{}", query.join("&")) };
        let mut body = self.send("GET", &path, None, false)?.into_body();
        body.set_timeout(None).map_err(|e| DockerError::Protocol { method: "GET", path, message: e.to_string() })?;
        Ok(Events { lines: BufReader::new(body).lines() })
    }

    /// Run a command inside a running container and wait for it (no TTY, no stdin).
    pub fn exec(&self, container: &str, cmd: &[&str], options: &ExecOptions) -> Result<ExecOutput> {
        let create_path = format!("/containers/{}/exec", http::encode(container));
        let mut spec = json!({ "AttachStdout": true, "AttachStderr": true, "Tty": false, "Cmd": cmd });
        if let Some(user) = &options.user {
            spec["User"] = json!(user);
        }
        if !options.env.is_empty() {
            spec["Env"] = json!(options.env);
        }
        if let Some(dir) = &options.workdir {
            spec["WorkingDir"] = json!(dir);
        }
        let protocol = |method: &'static str, path: &str, message: String| DockerError::Protocol {
            method,
            path: path.to_string(),
            message,
        };
        let raw = self
            .send("POST", &create_path, Some(&spec), false)?
            .bytes()
            .map_err(|e| protocol("POST", &create_path, e.to_string()))?;
        let created: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        let id = created["Id"]
            .as_str()
            .ok_or_else(|| protocol("POST", &create_path, "no exec id in the answer".into()))?
            .to_string();

        let start_path = format!("/exec/{id}/start");
        let mut output = Vec::new();
        self.send("POST", &start_path, Some(&json!({ "Detach": false, "Tty": false })), true)?
            .into_body()
            .read_to_end(&mut output)
            .map_err(|e| protocol("POST", &start_path, e.to_string()))?;
        let (stdout, stderr) = http::demux(&output);

        let state = self.get(&format!("/exec/{id}/json"))?;
        Ok(ExecOutput { exit_code: state["ExitCode"].as_i64().unwrap_or(-1), stdout, stderr })
    }

    /// A container's log output so far, as (stdout, stderr). TTY containers have everything in stdout.
    pub fn logs(&self, container: &str, options: &LogOptions) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut path = format!("/containers/{}/logs?stdout=true&stderr=true", http::encode(container));
        if let Some(since) = options.since {
            path.push_str(&format!("&since={since}"));
        }
        if let Some(tail) = options.tail {
            path.push_str(&format!("&tail={tail}"));
        }
        let raw = self.send("GET", &path, None, false)?.bytes().map_err(|e| DockerError::Protocol {
            method: "GET",
            path: path.clone(),
            message: e.to_string(),
        })?;
        Ok(http::demux(&raw))
    }
}
