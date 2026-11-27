//! The service: the one long-running process. It runs the checks on their intervals and the calendar
//! jobs at their times, tells systemd it is alive, and answers the CLI on `/run/moi.sock`.
//!
//! Spec: docs/spec/10-daemon.md. Plain threads, no async runtime: the daemon waits — for the next due
//! job, for a job to finish, for a connection — and every job is blocking work that already bounds
//! itself with timeouts. One thread sleeping on a channel costs nothing; a runtime would be the
//! largest dependency of the binary and buy no concurrency the jobs could use.

pub mod calendar;
mod jobs;
pub mod zone;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::checks::Check;
use crate::config::{Config, Loaded};
use crate::error::Error;
use crate::host::{self, Host};
use crate::log::Logger;
use crate::redact::Masker;
use crate::state::StateDir;
use calendar::Calendar;
use zone::Zone;

/// Where the CLI reaches the daemon.
pub const SOCKET: &str = "/run/moi.sock";
/// When the daily snapshot, audit and housekeeping run (local time): after a night's backup, before
/// anyone is up.
const DAILY_AT: &str = "05:00";
/// A calendar job missed while the machine was off runs this long after the daemon starts.
const CATCH_UP: i64 = 300;
/// Longest the loop sleeps, so a clock set from outside is noticed.
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// When a job runs.
#[derive(Debug, Clone)]
pub enum Schedule {
    /// Every so often, counted from the daemon's start; never two runs at once.
    Every(Duration),
    /// At calendar times; a run missed while the daemon was down is caught up.
    At(Calendar),
}

/// What a job does.
#[derive(Clone)]
pub enum Work {
    Check(Arc<dyn Check>),
    /// Snapshot, audit, housekeeping, drift report.
    Daily,
}

impl std::fmt::Debug for Work {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Work::Check(check) => write!(f, "Check({})", check.name()),
            Work::Daily => f.write_str("Daily"),
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    /// `up`/`down` for a check, `ok`/`failed` for other jobs.
    pub status: String,
    pub message: String,
    pub duration_ms: u64,
}

#[derive(Debug)]
pub struct Job {
    pub name: String,
    pub schedule: Schedule,
    /// When it is next due (seconds since the epoch).
    pub next: i64,
    pub running: bool,
    /// When it last finished, and how.
    pub last: Option<(i64, Outcome)>,
    work: Work,
}

/// What a job needs to do its work; shared with the threads jobs run on.
#[derive(Debug)]
pub struct Core {
    pub host: Arc<dyn Host>,
    pub config: Arc<Config>,
    pub masker: Masker,
    pub state: StateDir,
    pub zone: Zone,
}

#[derive(Debug)]
pub struct Daemon {
    core: Arc<Core>,
    jobs: Vec<Job>,
    started: i64,
}

/// Something the loop wakes up for.
enum Event {
    Done(usize, Outcome),
    /// The CLI asks for a job to run now.
    Trigger(String),
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Db(e.to_string())
}

impl Daemon {
    pub fn new(host: Arc<dyn Host>, loaded: Loaded) -> Result<Self, Error> {
        let state = StateDir::new(loaded.config.state.dir.clone());
        state.ensure(host.as_ref())?;
        let config = Arc::new(loaded.config);
        let core = Arc::new(Core { zone: Zone::load(host.as_ref()), host, config, masker: loaded.masker, state });
        let started = host::unix_seconds(core.host.now());
        let mut daemon = Self { core, jobs: Vec::new(), started };
        daemon.plan()?;
        Ok(daemon)
    }

    /// Set up the jobs the configuration asks for.
    fn plan(&mut self) -> Result<(), Error> {
        let core = Arc::clone(&self.core);
        let db = core.state.open_db(core.host.as_ref())?;
        let last_run = |job: &str| -> Option<i64> {
            db.conn().query_row("SELECT last_run FROM schedule_runs WHERE job = ?1", [job], |r| r.get(0)).ok()
        };
        // checks run right away, then on their interval
        for check in crate::checks::enabled(&core.config) {
            let interval = check.interval(&core.config);
            self.jobs.push(Job {
                name: format!("check:{}", check.name()),
                schedule: Schedule::Every(interval),
                next: self.started,
                running: false,
                last: None,
                work: Work::Check(check),
            });
        }
        let mut calendar = |name: &str, when: &str, work: Work| -> Result<(), Error> {
            let calendar = Calendar::parse(when)
                .ok_or_else(|| Error::Usage(format!("not a schedule moi can run: \"{when}\" (job {name})")))?;
            // from the last run on: an occurrence that has passed since then was missed
            let since = last_run(name).unwrap_or(self.started);
            let due = calendar.next_after(since, &core.zone).unwrap_or(i64::MAX);
            let next = if due <= self.started { self.started + CATCH_UP } else { due };
            self.jobs.push(Job {
                name: name.to_string(),
                schedule: Schedule::At(calendar),
                next,
                running: false,
                last: None,
                work,
            });
            Ok(())
        };
        calendar("daily", DAILY_AT, Work::Daily)?;
        Ok(())
    }

    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    fn now(&self) -> i64 {
        host::unix_seconds(self.core.host.now())
    }

    /// Jobs that should start now.
    fn due(&self, now: i64) -> Vec<usize> {
        (0..self.jobs.len()).filter(|i| !self.jobs[*i].running && self.jobs[*i].next <= now).collect()
    }

    /// A job ended: remember how, and decide when it runs next.
    fn finish(&mut self, index: usize, outcome: Outcome) -> Result<(), Error> {
        let now = self.now();
        let core = Arc::clone(&self.core);
        let job = &mut self.jobs[index];
        job.running = false;
        match &job.schedule {
            // from the time it was due, not from when it ended: no drift; ticks it ran through are skipped
            Schedule::Every(interval) => {
                let step = i64::try_from(interval.as_secs()).unwrap_or(i64::MAX).max(1);
                job.next += step * ((now - job.next).div_euclid(step) + 1);
            }
            Schedule::At(calendar) => job.next = calendar.next_after(now, &core.zone).unwrap_or(i64::MAX),
        }
        let db = core.state.open_db(core.host.as_ref())?;
        db.conn()
            .execute(
                "INSERT INTO schedule_runs (job, last_run, last_status) VALUES (?1, ?2, ?3)
                 ON CONFLICT(job) DO UPDATE SET last_run = excluded.last_run, last_status = excluded.last_status",
                (&job.name, now, &outcome.status),
            )
            .map_err(db_err)?;
        job.last = Some((now, outcome));
        Ok(())
    }

    /// Run every job that is due, one after the other, and return what ran. The service loop does the
    /// same on threads; this is the loop's logic without its waiting.
    pub fn tick(&mut self) -> Result<Vec<(String, Outcome)>, Error> {
        let mut ran = Vec::new();
        for index in self.due(self.now()) {
            self.jobs[index].running = true;
            let outcome = jobs::run(&self.core, &self.jobs[index].work);
            self.finish(index, outcome.clone())?;
            ran.push((self.jobs[index].name.clone(), outcome));
        }
        Ok(ran)
    }

    /// Make a job due now (`moi status` shows it running; the CLI's triggers end up here).
    pub fn trigger(&mut self, name: &str) -> bool {
        let now = self.now();
        match self.jobs.iter_mut().find(|j| j.name == name) {
            Some(job) => {
                job.next = job.next.min(now);
                true
            }
            None => false,
        }
    }

    /// What `moi status` shows.
    pub fn status(&self) -> Value {
        let time = |secs: i64| {
            host::format_rfc3339(std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or(0)))
        };
        let jobs: Vec<Value> = self
            .jobs
            .iter()
            .map(|j| {
                json!({
                    "name": j.name,
                    "running": j.running,
                    "next": time(j.next),
                    "last": j.last.as_ref().map(|(at, o)| json!({ "at": time(*at), "status": o.status, "message": o.message, "duration_ms": o.duration_ms })),
                })
            })
            .collect();
        json!({ "running": true, "started": time(self.started), "now": time(self.now()), "version": env!("CARGO_PKG_VERSION"), "jobs": jobs })
    }

    /// The line systemd shows for the unit: what comes next.
    fn status_line(&self) -> String {
        let mut next: Vec<(&i64, &str)> =
            self.jobs.iter().filter(|j| !j.running).map(|j| (&j.next, j.name.as_str())).collect();
        next.sort();
        let running = self.jobs.iter().filter(|j| j.running).count();
        let upcoming: Vec<String> = next
            .iter()
            .take(3)
            .map(|(at, name)| {
                format!(
                    "{name} {}",
                    crate::state::history::display_time(&host::format_rfc3339(
                        std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(**at).unwrap_or(0))
                    ))
                )
            })
            .collect();
        format!("{} jobs, {running} running; next: {}", self.jobs.len(), upcoming.join(", "))
    }

    /// The service loop. Returns only on an error that makes going on pointless.
    pub fn run(mut self) -> Result<(), Error> {
        let host = Arc::clone(&self.core.host);
        let log = Logger::new(Arc::clone(&host), "moi").with_masker(self.core.masker.clone());
        let (events, inbox) = mpsc::channel::<Event>();
        let shared = Arc::new(Mutex::new(self.status()));
        // the CLI's side door
        let listener = host
            .listen_unix(Path::new(SOCKET), 0o600)
            .map_err(|e| Error::host(format!("cannot listen on {SOCKET}"), e))?;
        {
            let (shared, events) = (Arc::clone(&shared), events.clone());
            thread::Builder::new()
                .name("moi-ipc".into())
                .spawn(move || {
                    loop {
                        let Ok(conn) = listener.accept() else { continue };
                        answer(conn, &shared, &events);
                    }
                })
                .map_err(Error::Io)?;
        }
        // systemd's watchdog: pinged at half the interval it allows, by this loop only — if the loop
        // hangs, the pings stop and systemd restarts the service
        let watchdog = host
            .env("WATCHDOG_USEC")
            .and_then(|usec| usec.parse::<u64>().ok())
            .map(|usec| Duration::from_micros(usec / 2));
        log.info(&format!("started: {}", self.status_line()), &[]);
        let _ = host.notify("READY=1");
        loop {
            let now = self.now();
            for index in self.due(now) {
                self.jobs[index].running = true;
                let (core, work, done) = (Arc::clone(&self.core), self.jobs[index].work.clone(), events.clone());
                let spawned = thread::Builder::new().name(format!("moi-{}", self.jobs[index].name)).spawn(move || {
                    let outcome = jobs::run(&core, &work);
                    let _ = done.send(Event::Done(index, outcome));
                });
                if let Err(e) = spawned {
                    self.finish(
                        index,
                        Outcome { status: "failed".into(), message: format!("could not start: {e}"), duration_ms: 0 },
                    )?;
                }
            }
            *shared.lock().unwrap_or_else(PoisonError::into_inner) = self.status();
            let _ = host.notify(&format!("WATCHDOG=1\nSTATUS={}", self.status_line()));
            // sleep until the next job is due, something happens, or the watchdog wants to hear from us
            let next = self.jobs.iter().filter(|j| !j.running).map(|j| j.next).min().unwrap_or(i64::MAX);
            let until_next = Duration::from_secs(u64::try_from(next.saturating_sub(now)).unwrap_or(0));
            let wait = until_next.min(watchdog.unwrap_or(MAX_SLEEP)).min(MAX_SLEEP);
            match inbox.recv_timeout(wait) {
                Ok(Event::Done(index, outcome)) => {
                    if matches!(outcome.status.as_str(), "failed") {
                        log.warn(
                            &format!("{} failed: {}", self.jobs[index].name, outcome.message),
                            &[("job", &self.jobs[index].name)],
                        );
                    }
                    self.finish(index, outcome)?;
                }
                Ok(Event::Trigger(name)) => {
                    self.trigger(&name);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }
}

/// Serve one CLI connection: one JSON request line, one JSON answer line.
fn answer(conn: Box<dyn host::Conn>, shared: &Arc<Mutex<Value>>, events: &mpsc::Sender<Event>) {
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    if reader.get_mut().set_timeout(Some(Duration::from_secs(5))).is_err() || reader.read_line(&mut line).is_err() {
        return;
    }
    let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    let status = || shared.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let response = match request["cmd"].as_str() {
        Some("status") => status(),
        Some("run") => {
            let name = request["job"].as_str().unwrap_or_default();
            let known = status()["jobs"].as_array().is_some_and(|jobs| jobs.iter().any(|j| j["name"] == name));
            if known && events.send(Event::Trigger(name.to_string())).is_ok() {
                json!({ "ok": true })
            } else {
                json!({ "ok": false, "error": format!("no such job: {name}") })
            }
        }
        _ => json!({ "ok": false, "error": "unknown request" }),
    };
    let _ = writeln!(reader.get_mut(), "{response}");
}

/// Ask the running daemon something. `None` when no daemon answers.
pub fn ask(host: &dyn Host, request: &Value) -> Option<Value> {
    let mut conn = host.connect_unix(Path::new(SOCKET)).ok()?;
    conn.set_timeout(Some(Duration::from_secs(5))).ok()?;
    writeln!(conn, "{request}").ok()?;
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}
