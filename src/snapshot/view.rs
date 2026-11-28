//! Reading a snapshot back: the state dir as the audit, the context file and the notes see it. Missing
//! files read as empty, so every consumer works on a server that has no Docker, no cron, no Incus.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::host::Host;
use crate::state::StateDir;

/// A snapshot on disk.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    host: &'a dyn Host,
    state: &'a StateDir,
}

/// One scheduled command from a crontab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronJob {
    /// `crontab -u <user>` or the file it is in.
    pub source: String,
    /// Five fields, or an `@keyword`.
    pub schedule: String,
    pub user: String,
    pub command: String,
}

impl<'a> View<'a> {
    pub fn new(host: &'a dyn Host, state: &'a StateDir) -> Self {
        Self { host, state }
    }

    pub fn host(&self) -> &'a dyn Host {
        self.host
    }

    pub fn state(&self) -> &'a StateDir {
        self.state
    }

    /// True once a snapshot was taken.
    pub fn exists(&self) -> bool {
        self.host.exists(&self.state.snapshot())
    }

    fn read(&self, path: PathBuf) -> String {
        self.host.read_to_string(&path).unwrap_or_default()
    }

    /// A generated file under `snapshot/`.
    pub fn text(&self, rel: &str) -> String {
        self.read(self.state.snapshot().join(rel))
    }

    /// A generated file under `volatile/`.
    pub fn volatile(&self, rel: &str) -> String {
        self.read(self.state.volatile().join(rel))
    }

    pub fn lines(&self, rel: &str) -> Vec<String> {
        self.text(rel).lines().map(str::to_string).filter(|l| !l.is_empty()).collect()
    }

    pub fn volatile_lines(&self, rel: &str) -> Vec<String> {
        self.volatile(rel).lines().map(str::to_string).filter(|l| !l.is_empty()).collect()
    }

    /// A JSON file under `snapshot/`; `Null` if absent.
    pub fn json(&self, rel: &str) -> Value {
        serde_json::from_str(&self.text(rel)).unwrap_or(Value::Null)
    }

    /// A JSON list under `snapshot/`; empty if absent.
    /// The names in a directory of the snapshot, sorted.
    pub fn dir(&self, rel: &str) -> Vec<String> {
        let mut names = self.host.read_dir(&self.state.snapshot().join(rel)).unwrap_or_default();
        names.sort();
        names
    }

    pub fn list(&self, rel: &str) -> Vec<Value> {
        match self.json(rel) {
            Value::Array(items) => items,
            _ => Vec::new(),
        }
    }

    /// The captured copy of a host file, if it was captured and is text.
    pub fn file(&self, host_path: &str) -> Option<String> {
        let copy = self.state.snapshot().join("files").join(host_path.trim_start_matches('/'));
        let data = self.host.read(&copy).ok()?;
        (!data.iter().take(8192).any(|b| *b == 0)).then(|| String::from_utf8_lossy(&data).into_owned())
    }

    /// True if the host file (or directory) was captured.
    pub fn captured(&self, host_path: &str) -> bool {
        self.host.exists(&self.state.snapshot().join("files").join(host_path.trim_start_matches('/')))
    }

    /// Captured files directly inside a host directory, as host paths, sorted.
    pub fn captured_in(&self, host_dir: &str) -> Vec<String> {
        let dir = self.state.snapshot().join("files").join(host_dir.trim_start_matches('/'));
        self.host
            .read_dir(&dir)
            .unwrap_or_default()
            .into_iter()
            .map(|name| format!("{}/{name}", host_dir.trim_end_matches('/')))
            .collect()
    }

    pub fn hostname(&self) -> String {
        let name = self.text("meta/hostname").trim().to_string();
        if name.is_empty() { crate::host::short_hostname(self.host) } else { name }
    }

    pub fn containers(&self) -> Vec<Value> {
        self.list("docker/containers.json")
    }

    pub fn services(&self) -> Vec<Value> {
        self.list("docker/services.json")
    }

    pub fn networks(&self) -> Vec<Value> {
        self.list("docker/networks.json")
    }

    pub fn volumes(&self) -> Vec<Value> {
        self.list("docker/volumes.json")
    }

    pub fn compose_projects(&self) -> Vec<Value> {
        self.list("docker/compose-projects.json")
    }

    /// Names of the Incus instances.
    pub fn incus_instances(&self) -> Vec<String> {
        self.host
            .read_dir(&self.state.snapshot().join("incus/instances"))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|f| f.strip_suffix(".yaml").map(str::to_string))
            .collect()
    }

    /// Container name → state (`running`, `exited`, …) as last seen.
    pub fn container_states(&self) -> BTreeMap<String, String> {
        self.volatile_lines("docker-ps.tsv")
            .iter()
            .filter_map(|l| {
                let mut f = l.split('\t');
                Some((f.next()?.to_string(), f.next()?.to_string()))
            })
            .collect()
    }

    /// The owner's unit files (`/etc/systemd/system/x.service`, drop-ins included).
    pub fn custom_units(&self) -> Vec<String> {
        self.lines("systemd/custom-units.txt")
    }

    /// The directives of a captured unit file that describe what it does.
    pub fn unit_info(&self, unit_path: &str) -> BTreeMap<String, Vec<String>> {
        const KEYS: [&str; 16] = [
            "Description",
            "ExecStart",
            "ExecStartPre",
            "User",
            "WorkingDirectory",
            "OnCalendar",
            "OnBootSec",
            "OnUnitActiveSec",
            "Requires",
            "After",
            "What",
            "Where",
            "Type",
            "PathChanged",
            "PathModified",
            "Unit",
        ];
        let mut info: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for line in self.file(unit_path).unwrap_or_default().lines() {
            if let Some((key, value)) = line.split_once('=')
                && KEYS.contains(&key.trim())
            {
                info.entry(key.trim().to_string()).or_default().push(value.trim().to_string());
            }
        }
        info
    }

    /// Every cron job: user crontabs, `/etc/crontab` and `/etc/cron.d/*` (the stock `run-parts` lines are
    /// left out; the directories they run are listed separately).
    pub fn cron_jobs(&self) -> Vec<CronJob> {
        let mut out = Vec::new();
        for name in self.host.read_dir(&self.state.snapshot().join("cron")).unwrap_or_default() {
            let Some(user) = name.strip_prefix("user-").and_then(|n| n.strip_suffix(".crontab")) else { continue };
            for (schedule, _, command) in parse_crontab(&self.text(&format!("cron/{name}")), false) {
                out.push(CronJob { source: format!("crontab -u {user}"), schedule, user: user.to_string(), command });
            }
        }
        let mut system = vec!["/etc/crontab".to_string()];
        system.extend(self.captured_in("/etc/cron.d"));
        for file in system {
            for (schedule, user, command) in parse_crontab(&self.file(&file).unwrap_or_default(), true) {
                if !(command.contains("run-parts") && command.contains("/etc/cron.")) {
                    out.push(CronJob { source: file.clone(), schedule, user, command });
                }
            }
        }
        out
    }

    /// Scripts from the index that a cron job or one of the owner's units actually runs.
    pub fn scheduled_scripts(&self) -> Vec<String> {
        let mut text: String = self.cron_jobs().iter().map(|j| format!("{}\n", j.command)).collect();
        for unit in self.custom_units() {
            text.push_str(&self.file(&unit).unwrap_or_default());
            text.push('\n');
        }
        self.lines("scripts/index.txt").into_iter().filter(|s| text.contains(s.as_str())).collect()
    }

    /// Who runs a script: cron lines and units that mention it, as `(source, schedule or directive)`.
    pub fn callers(&self, script: &str) -> Vec<String> {
        let mut out = Vec::new();
        for job in self.cron_jobs().iter().filter(|j| j.command.contains(script)) {
            out.push(format!("cron ({}, {})", job.source, job.schedule));
        }
        for unit in self.custom_units() {
            if self
                .file(&unit)
                .is_some_and(|t| t.lines().any(|l| !l.trim_start().starts_with('#') && l.contains(script)))
            {
                out.push(Path::new(&unit).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(unit));
            }
        }
        out
    }
}

/// `(schedule, user, command)` of each job line. System crontabs have a user column, user crontabs do
/// not (their user is filled in by the caller).
pub fn parse_crontab(text: &str, system: bool) -> Vec<(String, String, String)> {
    let mut jobs = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        // NAME=value lines set the environment of the jobs below
        let name_len = line.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(line.len());
        if name_len > 0
            && line[name_len..].trim_start().starts_with('=')
            && !line.starts_with(|c: char| c.is_ascii_digit())
        {
            continue;
        }
        let schedule_fields = if line.starts_with('@') { 1 } else { 5 };
        let mut rest = line;
        let mut fields = Vec::new();
        for _ in 0..schedule_fields + usize::from(system) {
            let trimmed = rest.trim_start();
            let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
            fields.push(&trimmed[..end]);
            rest = &trimmed[end..];
        }
        let user = if system { fields.pop().unwrap_or("?").to_string() } else { String::new() };
        jobs.push((fields.join(" "), user, rest.trim().to_string()));
    }
    jobs
}
