//! A fake Docker engine for `FixtureHost`: answers Engine API requests from the fixture's `docker/`
//! directory. `GET /x/y?query` is served from `docker/x/y.json` (the query is ignored); see
//! `tests/fixtures/README.md` for events, exec and logs. Every request is recorded so tests can assert
//! what moi asked for — and that it changed nothing.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::{Conn, FixtureHost};

/// What tests can change about the fake engine at run time.
#[derive(Debug, Default)]
pub(crate) struct DockerState {
    /// Every request line received: `GET /containers/json?all=true`.
    pub requests: Vec<String>,
    /// Request path (no query) → JSON body, shadowing the fixture's files.
    pub overrides: BTreeMap<String, String>,
}

#[derive(Debug)]
pub(crate) struct FakeConn {
    /// `docker/` directories, most specific first (a parity case, then the fixture host).
    dirs: Vec<PathBuf>,
    state: Arc<Mutex<DockerState>>,
    request: Vec<u8>,
    response: Option<io::Cursor<Vec<u8>>>,
}

impl FakeConn {
    pub fn new(dirs: Vec<PathBuf>, state: Arc<Mutex<DockerState>>) -> Self {
        Self { dirs, state, request: Vec::new(), response: None }
    }

    /// Where a fixture file is: in the first directory that has it (else where the last would have it).
    fn file(&self, relative: impl AsRef<Path>) -> PathBuf {
        let relative = relative.as_ref();
        self.dirs
            .iter()
            .map(|d| d.join(relative))
            .find(|p| p.exists())
            .unwrap_or_else(|| self.dirs.last().map(|d| d.join(relative)).unwrap_or_default())
    }
}

impl Write for FakeConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.request.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for FakeConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.response.is_none() {
            let response = self.answer();
            self.response = Some(io::Cursor::new(response));
        }
        match &mut self.response {
            Some(cursor) => cursor.read(buf),
            None => Ok(0),
        }
    }
}

impl Conn for FakeConn {
    fn set_timeout(&mut self, _timeout: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}

fn respond(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nApi-Version: 1.52\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn json(body: &[u8]) -> Vec<u8> {
    respond("200 OK", "application/json", body)
}

fn not_found(message: &str) -> Vec<u8> {
    respond("404 Not Found", "application/json", serde_json::json!({ "message": message }).to_string().as_bytes())
}

/// One frame of Docker's multiplexed stream.
fn frame(stream: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![stream, 0, 0, 0];
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(data);
    out
}

impl FakeConn {
    fn answer(&mut self) -> Vec<u8> {
        let text = String::from_utf8_lossy(&self.request).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let mut first = head.lines().next().unwrap_or_default().split_whitespace();
        let (method, target) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
        let path = target.split('?').next().unwrap_or_default().to_string();
        let override_body = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.requests.push(format!("{method} {target}"));
            state.overrides.get(&path).cloned()
        };
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        // `docker/fail`: requests this engine refuses, as `METHOD /path-prefix status message`
        for line in fs::read_to_string(self.file("fail")).unwrap_or_default().lines() {
            let mut words = line.splitn(4, ' ');
            if let (Some(m), Some(prefix), Some(status), message) =
                (words.next(), words.next(), words.next(), words.next())
                && m == method
                && path.starts_with(prefix)
            {
                let body = serde_json::json!({ "message": message.unwrap_or("refused by the fixture") }).to_string();
                return respond(&format!("{status} Error"), "application/json", body.as_bytes());
            }
        }

        match (method, segments.as_slice()) {
            ("GET", ["_ping"]) => respond("200 OK", "text/plain", b"OK"),
            ("GET", ["events"]) => self.events(),
            // a created container is known by the name it was given
            ("POST", ["containers", "create"]) => {
                let name = target.split("name=").nth(1).unwrap_or("created").split('&').next().unwrap_or("created");
                respond(
                    "201 Created",
                    "application/json",
                    serde_json::json!({ "Id": name, "Warnings": [] }).to_string().as_bytes(),
                )
            }
            ("POST", ["containers", container, "exec"]) => self.exec_create(container, body),
            ("POST", ["exec", id, "start"]) => self.exec_start(id),
            ("GET", ["exec", id, "json"]) => {
                let exit =
                    fs::read_to_string(self.exec_file(id, "exit")).ok().and_then(|s| s.trim().parse::<i64>().ok());
                json(
                    serde_json::json!({ "ID": id, "Running": false, "ExitCode": exit.unwrap_or(0) })
                        .to_string()
                        .as_bytes(),
                )
            }
            ("GET", ["containers", container, "logs"]) => {
                let dir = self.container_dir(container);
                let stdout =
                    fs::read(self.file(Path::new("containers").join(&dir).join("logs.txt"))).unwrap_or_default();
                let mut out = frame(1, &stdout);
                out.extend(frame(
                    2,
                    &fs::read(self.file(Path::new("containers").join(&dir).join("logs.err"))).unwrap_or_default(),
                ));
                respond("200 OK", "application/vnd.docker.multiplexed-stream", &out)
            }
            // `docker service ls` counts: `docker/service-status.json` maps a service to "running/wanted";
            // a service not listed there runs every replica it wants
            ("GET", ["services"]) if target.contains("status=true") => {
                let read = |name: &str| {
                    fs::read(self.file(name))
                        .ok()
                        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
                };
                let mut services = read("services.json").unwrap_or_else(|| serde_json::json!([]));
                let status = read("service-status.json").unwrap_or_default();
                for service in services.as_array_mut().into_iter().flatten() {
                    let wanted = service["Spec"]["Mode"]["Replicated"]["Replicas"].as_u64().unwrap_or(1);
                    let counts = status[service["Spec"]["Name"].as_str().unwrap_or_default()].as_str().and_then(|s| {
                        let (running, wanted) = s.split_once('/')?;
                        Some((running.parse::<u64>().ok()?, wanted.parse::<u64>().ok()?))
                    });
                    let (running, wanted) = counts.unwrap_or((wanted, wanted));
                    service["ServiceStatus"] = serde_json::json!({ "RunningTasks": running, "DesiredTasks": wanted });
                }
                json(services.to_string().as_bytes())
            }
            ("GET", _) => {
                if let Some(body) = override_body {
                    return json(body.as_bytes());
                }
                let mut relative: Vec<String> = segments.iter().map(ToString::to_string).collect();
                if let ["containers", container, ..] = segments.as_slice() {
                    relative[1] = self.container_dir(container);
                }
                let file = self.file(format!("{}.json", relative.join("/")));
                match fs::read(&file) {
                    Ok(body) => json(&body),
                    Err(_) => not_found(&format!("no fixture for {path}: add {}", file.display())),
                }
            }
            // Changes are accepted and recorded; the fixture itself never changes.
            _ => respond("204 No Content", "text/plain", b""),
        }
    }

    /// The directory under `docker/containers/` for a container given by id, id prefix or name.
    fn container_dir(&self, reference: &str) -> String {
        if self.file(Path::new("containers").join(reference)).is_dir() {
            return reference.to_string();
        }
        let list = fs::read(self.file("containers/json.json")).unwrap_or_default();
        let list: serde_json::Value = serde_json::from_slice(&list).unwrap_or_default();
        let wanted = format!("/{reference}");
        list.as_array()
            .into_iter()
            .flatten()
            .find(|c| {
                c["Id"].as_str().is_some_and(|id| id.starts_with(reference))
                    || c["Names"].as_array().is_some_and(|names| names.iter().any(|n| n == wanted.as_str()))
            })
            .and_then(|c| c["Id"].as_str())
            .unwrap_or(reference)
            .to_string()
    }

    /// A container's name (exec fixtures are filed by name), given its name, id or id prefix.
    fn container_name(&self, reference: &str) -> String {
        let id = self.container_dir(reference);
        let list = fs::read(self.file("containers/json.json")).unwrap_or_default();
        let list: serde_json::Value = serde_json::from_slice(&list).unwrap_or_default();
        list.as_array()
            .into_iter()
            .flatten()
            .find(|c| c["Id"] == id.as_str())
            .and_then(|c| c["Names"][0].as_str())
            .map_or_else(|| reference.to_string(), |n| n.trim_start_matches('/').to_string())
    }

    fn events(&self) -> Vec<u8> {
        let lines = fs::read_to_string(self.file("events.jsonl")).unwrap_or_default();
        let mut out =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for line in lines.lines().filter(|l| !l.trim().is_empty()) {
            out.extend_from_slice(format!("{:x}\r\n{line}\n\r\n", line.len() + 1).as_bytes());
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    }

    /// Exec ids are `<command key>-<container>`, so the three exec calls need no shared state.
    fn exec_file(&self, id: &str, ext: &str) -> PathBuf {
        let (key, container) = id.split_once('-').unwrap_or((id, ""));
        self.file(Path::new("exec").join(container).join(format!("{key}.{ext}")))
    }

    fn exec_create(&self, container: &str, body: &str) -> Vec<u8> {
        let spec: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
        let line: Vec<&str> = spec["Cmd"].as_array().into_iter().flatten().filter_map(|a| a.as_str()).collect();
        let line = line.join(" ");
        let container = &self.container_name(container);
        let id = format!("{}-{container}", FixtureHost::command_key(&line));
        let file = self.exec_file(&id, "out");
        if !file.exists() {
            return not_found(&format!("no canned exec output for `{line}` in {container}: add {}", file.display()));
        }
        respond("201 Created", "application/json", serde_json::json!({ "Id": id }).to_string().as_bytes())
    }

    fn exec_start(&self, id: &str) -> Vec<u8> {
        let mut out =
            b"HTTP/1.1 101 UPGRADED\r\nContent-Type: application/vnd.docker.multiplexed-stream\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n"
                .to_vec();
        out.extend(frame(1, &fs::read(self.exec_file(id, "out")).unwrap_or_default()));
        let stderr = fs::read(self.exec_file(id, "err")).unwrap_or_default();
        if !stderr.is_empty() {
            out.extend(frame(2, &stderr));
        }
        out
    }
}
