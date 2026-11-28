//! Health checks: the engine, the built-ins and custom scripts.
//!
//! Spec: docs/spec/05-checks.md. A check reads the machine through `host::Host` and the Docker API and
//! returns a status and a message. The built-ins reproduce the messages of the legacy scripts byte for
//! byte (`tests/parity/`). What a check must remember between runs — CPU counters, restart counts,
//! how many runs in a row a condition has held — lives in `moi.db` and is only written by recorded runs
//! (the daemon's); a manual `moi check` reads it and leaves it alone.

mod custom;
mod push;
mod resources;
mod services;
mod ssh;
mod storage;

use std::collections::BTreeMap;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

pub use push::push;

use crate::config::Config;
use crate::docker::Docker;
use crate::error::Error;
use crate::host::{self, Host};
use crate::state::db::Db;

/// Results kept per check in `moi.db`.
const HISTORY: i64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Up,
    Down,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Up => "up",
            Status::Down => "down",
        }
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: Status,
    pub message: String,
}

impl Outcome {
    pub fn up(message: impl Into<String>) -> Self {
        Self { status: Status::Up, message: message.into() }
    }

    pub fn down(message: impl Into<String>) -> Self {
        Self { status: Status::Down, message: message.into() }
    }
}

/// What a check remembers between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Memory {
    /// Values of the previous run (CPU counters, restart counts).
    pub values: BTreeMap<String, String>,
    /// Condition → number of consecutive runs it has held.
    pub sustained: BTreeMap<String, u32>,
}

impl Memory {
    /// Note whether a condition holds in this run, and say whether it has now held for `runs` runs in a
    /// row — only then is it reported. A condition that does not hold is forgotten.
    pub fn sustain(&mut self, condition: &str, holds: bool, runs: u32) -> bool {
        if !holds {
            self.sustained.remove(condition);
            return false;
        }
        let count = self.sustained.entry(condition.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        *count >= runs.max(1)
    }

    /// Load a check's memory.
    pub fn load(db: &Db, check: &str) -> Result<Self, Error> {
        let err = |e: rusqlite::Error| Error::Db(e.to_string());
        let mut memory = Self::default();
        let mut values = db.conn().prepare("SELECT key, value FROM check_values WHERE check_name = ?1").map_err(err)?;
        for row in values.query_map([check], |r| Ok((r.get(0)?, r.get(1)?))).map_err(err)? {
            let (key, value): (String, String) = row.map_err(err)?;
            memory.values.insert(key, value);
        }
        let mut sustained =
            db.conn().prepare("SELECT condition, runs FROM sustained WHERE check_name = ?1").map_err(err)?;
        for row in sustained.query_map([check], |r| Ok((r.get(0)?, r.get(1)?))).map_err(err)? {
            let (condition, runs): (String, u32) = row.map_err(err)?;
            memory.sustained.insert(condition, runs);
        }
        Ok(memory)
    }

    /// Replace a check's stored memory with this one.
    pub fn save(&self, db: &Db, check: &str, now: i64) -> Result<(), Error> {
        let err = |e: rusqlite::Error| Error::Db(e.to_string());
        let tx = db.conn().unchecked_transaction().map_err(err)?;
        tx.execute("DELETE FROM check_values WHERE check_name = ?1", [check]).map_err(err)?;
        for (key, value) in &self.values {
            tx.execute(
                "INSERT INTO check_values (check_name, key, value, at) VALUES (?1, ?2, ?3, ?4)",
                (check, key, value, now),
            )
            .map_err(err)?;
        }
        // keep `since` of conditions that were already being counted
        let since: BTreeMap<String, i64> = {
            let mut stmt = tx.prepare("SELECT condition, since FROM sustained WHERE check_name = ?1").map_err(err)?;
            stmt.query_map([check], |r| Ok((r.get(0)?, r.get(1)?))).map_err(err)?.flatten().collect()
        };
        tx.execute("DELETE FROM sustained WHERE check_name = ?1", [check]).map_err(err)?;
        for (condition, runs) in &self.sustained {
            let since = since.get(condition).copied().unwrap_or(now);
            tx.execute(
                "INSERT INTO sustained (check_name, condition, runs, since) VALUES (?1, ?2, ?3, ?4)",
                (check, condition, runs, since),
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)
    }
}

/// What a check may use.
#[derive(Debug, Clone)]
pub struct Cx {
    pub host: Arc<dyn Host>,
    pub config: Arc<Config>,
    pub docker: Docker,
    /// Where `moi.db` is (heal cooldowns are recorded the moment a rule acts).
    pub state: crate::state::StateDir,
    pub mode: Mode,
}

/// A health check.
pub trait Check: Send + Sync {
    /// The name it is run and recorded under.
    fn name(&self) -> String;

    /// Longest it may run.
    fn timeout(&self, config: &Config) -> Duration;

    /// How often the daemon runs it.
    fn interval(&self, config: &Config) -> Duration;

    /// The Uptime Kuma monitor it reports to, if any.
    fn monitor(&self, config: &Config) -> Option<String>;

    /// `SYSLOG_IDENTIFIER` of its journal entries.
    fn journal_tag(&self, config: &Config) -> String;

    /// Look at the machine. `memory` holds what the previous recorded run left and takes what this run
    /// wants remembered.
    fn run(&self, cx: &Cx, memory: &mut Memory) -> Outcome;
}

/// The enabled checks, in the order they are run and listed.
pub fn enabled(config: &Config) -> Vec<Arc<dyn Check>> {
    let mut checks: Vec<Arc<dyn Check>> = Vec::new();
    if config.checks.resources.common.enabled {
        checks.push(Arc::new(resources::Resources));
    }
    if config.checks.storage.common.enabled {
        checks.push(Arc::new(storage::Storage));
    }
    // the edge is part of the services message unless it reports to a monitor of its own
    let edge = &config.checks.edge;
    let own_edge = edge.enabled && (edge.monitor.is_some() || !config.checks.services.common.enabled);
    if config.checks.services.common.enabled {
        checks.push(Arc::new(services::Services {
            part: if own_edge { services::Part::Internal } else { services::Part::All },
        }));
    }
    if own_edge {
        checks.push(Arc::new(services::Services { part: services::Part::Edge }));
    }
    // the tunnel `ssh <server>` comes in by (16); provider `none` has nothing of moi's to watch
    if config.ssh.enabled
        && config.ssh.provider == crate::config::SshProvider::Cloudflare
        && config.ssh.hostname.is_some()
    {
        checks.push(Arc::new(ssh::Ssh));
    }
    for (index, custom) in config.checks.custom.iter().enumerate() {
        if custom.common.enabled {
            checks.push(Arc::new(custom::Custom { index, name: custom.name.clone() }));
        }
    }
    checks
}

/// Whether a run counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Look only: nothing is stored, so a manual run cannot disturb the daemon's deltas and counters.
    Dry,
    /// A scheduled run: memory and the result are stored.
    Record,
}

/// One finished check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub status: Status,
    pub message: String,
    pub duration_ms: u64,
    /// The monitor it reports to, if configured.
    pub monitor: Option<String>,
    /// `None` when no push was attempted; otherwise `"ok"` or why it failed.
    pub pushed: Option<String>,
}

/// Run a closure on its own thread and give up waiting after `timeout`. The thread is not killed: what
/// it was blocked on (a hung USB disk, a dead socket) keeps it until that returns.
pub(crate) fn with_timeout<T: Send + 'static>(
    timeout: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("moi-check".into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()?;
    rx.recv_timeout(timeout).ok()
}

/// Run one check. In [`Mode::Record`] its memory and result are stored in `moi.db`.
pub fn run(
    host: &Arc<dyn Host>,
    config: &Arc<Config>,
    db: &Db,
    check: &Arc<dyn Check>,
    mode: Mode,
) -> Result<CheckResult, Error> {
    let name = check.name();
    let mut memory = Memory::load(db, &name)?;
    let cx = Cx {
        host: Arc::clone(host),
        config: Arc::clone(config),
        docker: Docker::new(Arc::clone(host)),
        state: crate::state::StateDir::new(config.state.dir.clone()),
        mode,
    };
    let timeout = check.timeout(config);
    let started = Instant::now();
    let worker = Arc::clone(check);
    let finished = with_timeout(timeout, move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run(&cx, &mut memory)));
        (outcome, memory)
    });
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // A check that dies still reports: the legacy scripts pushed nothing and Kuma only noticed later.
    let (outcome, memory) = match finished {
        Some((Ok(outcome), memory)) => (outcome, Some(memory)),
        Some((Err(panic), _)) => {
            let why = panic
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .or_else(|| panic.downcast_ref::<String>().cloned());
            (Outcome::down(format!("check crashed: {}", why.unwrap_or_else(|| "unknown error".to_string()))), None)
        }
        None => (Outcome::down(format!("check crashed: timed out after {}s", timeout.as_secs())), None),
    };
    if mode == Mode::Record {
        let now = host::unix_seconds(host.now());
        if let Some(memory) = &memory {
            memory.save(db, &name, now)?;
        }
        let err = |e: rusqlite::Error| Error::Db(e.to_string());
        db.conn()
            .execute(
                "INSERT INTO check_results (check_name, at, status, message, duration_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                (&name, now, outcome.status.as_str(), &outcome.message, i64::try_from(duration_ms).unwrap_or(i64::MAX)),
            )
            .map_err(err)?;
        db.conn()
            .execute(
                "DELETE FROM check_results WHERE check_name = ?1 AND id NOT IN (SELECT id FROM check_results WHERE check_name = ?1 ORDER BY id DESC LIMIT ?2)",
                (&name, HISTORY),
            )
            .map_err(err)?;
    }
    Ok(CheckResult {
        name,
        status: outcome.status,
        message: outcome.message,
        duration_ms,
        monitor: check.monitor(config),
        pushed: None,
    })
}

/// Lines of the kernel log within the last `window` that match `matches` (case-insensitively: the
/// closure gets the lower-cased line). Zero when the journal cannot be read.
pub(crate) fn kernel_log_matches(host: &dyn Host, window: Duration, matches: impl Fn(&str) -> bool) -> usize {
    let secs = window.as_secs();
    let since = if secs.is_multiple_of(60) { format!("-{}min", secs / 60) } else { format!("-{secs}s") };
    let cmd = host::Cmd::new("journalctl")
        .args(["-k", "--since", since.as_str(), "--no-pager", "-q"])
        .timeout(Duration::from_secs(20));
    match host.run(&cmd) {
        Ok(out) => out.stdout_str().lines().filter(|l| matches(&l.to_lowercase())).count(),
        Err(_) => 0,
    }
}
