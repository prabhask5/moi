//! The service: the one long-running process. It runs the checks on their intervals and the calendar
//! jobs at their times, tells systemd it is alive, and answers the CLI on `/run/moi.sock`.
//!
//! Spec: docs/spec/10-daemon.md. Plain threads, no async runtime: the daemon waits — for the next due
//! job, for a job to finish, for a connection — and every job is blocking work that already bounds
//! itself with timeouts. One thread sleeping on a channel costs nothing; a runtime would be the
//! largest dependency of the binary and buy no concurrency the jobs could use.

pub mod calendar;
mod jobs;
pub mod watch;
pub mod zone;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::checks::Check;
use crate::config::{Config, HealRule, Loaded, Paths, When};
use crate::error::Error;
use crate::host::{self, Host};
use crate::log::Logger;
use crate::redact::Masker;
use crate::state::StateDir;
use calendar::Calendar;
pub use jobs::compose_dirs;
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
    /// When something asks for it: a change on the machine, a heal trigger.
    OnDemand,
}

/// What a job does.
#[derive(Clone)]
pub enum Work {
    Check(Arc<dyn Check>),
    /// Snapshot, audit, housekeeping, drift report.
    Daily,
    /// The nightly backup.
    Backup,
    /// The restore drill.
    Drill,
    /// A snapshot because something changed, and the container monitors that follow from it.
    Snapshot,
    /// A heal rule whose trigger fired, and what fired it.
    Heal(Box<HealRule>, String),
    /// Once after the machine started: firewall, mounts, compose stacks.
    Boot,
    /// The check after a boot that everything that ran before it runs again.
    Verify,
    /// Put the managed chains back (Docker restarted and may have rebuilt its own).
    Firewall,
}

impl std::fmt::Debug for Work {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Work::Check(check) => write!(f, "Check({})", check.name()),
            Work::Daily => f.write_str("Daily"),
            Work::Backup => f.write_str("Backup"),
            Work::Drill => f.write_str("Drill"),
            Work::Snapshot => f.write_str("Snapshot"),
            Work::Heal(rule, _) => write!(f, "Heal({})", rule.name),
            Work::Boot => f.write_str("Boot"),
            Work::Verify => f.write_str("Verify"),
            Work::Firewall => f.write_str("Firewall"),
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
    /// Where the configuration came from, for reloading it.
    paths: Paths,
    jobs: Vec<Job>,
    started: i64,
    /// Changes seen and not yet snapshotted.
    changes: watch::Debounce,
    /// Running jobs: when each began, and whether it has been said that it is taking too long.
    since: std::collections::BTreeMap<String, (i64, bool)>,
}

/// A running job that is past the time it may take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overdue {
    pub job: String,
    pub running_for: Duration,
    pub limit: Duration,
    /// Past twice its limit: it will not come back, and only a restart of the service ends it.
    pub stuck: bool,
}

impl Work {
    /// How long a job may run before the daemon says so. A thread cannot be stopped from outside, so
    /// the limit is a report, and at twice the limit a restart. Backups and drills take as long as the
    /// data is big: no limit.
    fn limit(&self, config: &Config) -> Option<Duration> {
        let minutes = |m: u64| Some(Duration::from_secs(m * 60));
        match self {
            Work::Check(check) => Some(check.timeout(config) + Duration::from_secs(60)),
            Work::Daily | Work::Snapshot => minutes(30),
            Work::Boot | Work::Verify => minutes(20),
            Work::Heal(..) => minutes(15),
            Work::Firewall => minutes(5),
            Work::Backup | Work::Drill => None,
        }
    }
}

/// Something the loop wakes up for.
enum Event {
    Done(String, Outcome),
    /// The CLI asks for a job to run now.
    Trigger(String),
    Docker(crate::docker::Event),
    /// The engine's event stream is back after having been gone: Docker restarted.
    DockerBack,
    File(std::path::PathBuf),
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
        let mut daemon = Self {
            core,
            paths: loaded.paths,
            jobs: Vec::new(),
            started,
            changes: watch::Debounce::default(),
            since: Default::default(),
        };
        daemon.plan()?;
        Ok(daemon)
    }

    /// Read the configuration again (it changed on disk). A configuration that does not load is not
    /// taken: the daemon keeps running with the one it has.
    pub fn reload(&mut self) -> Result<(), Error> {
        let host = Arc::clone(&self.core.host);
        let loaded = crate::config::load(host.as_ref(), &self.paths)?;
        let state = StateDir::new(loaded.config.state.dir.clone());
        state.ensure(host.as_ref())?;
        let previous = std::mem::take(&mut self.jobs);
        self.core = Arc::new(Core {
            zone: Zone::load(host.as_ref()),
            host,
            config: Arc::new(loaded.config),
            masker: loaded.masker,
            state,
        });
        self.plan()?;
        // jobs that are still there keep their place in time and what they last did
        for job in &mut self.jobs {
            if let Some(old) = previous.iter().find(|old| old.name == job.name) {
                job.last = old.last.clone();
                job.running = old.running;
                if !matches!(job.schedule, Schedule::At(_)) {
                    job.next = old.next;
                }
            }
        }
        Ok(())
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
        if core.config.backup.enabled {
            calendar("backup", &core.config.backup.schedule, Work::Backup)?;
            if let Some(when) = &core.config.backup.drill {
                calendar("drill", when, Work::Drill)?;
            }
        }
        // on demand: the snapshot after a change, and the heal rules that fire on events
        let on_demand = |name: String, work: Work| Job {
            name,
            schedule: Schedule::OnDemand,
            next: i64::MAX,
            running: false,
            last: None,
            work,
        };
        self.jobs.push(on_demand("snapshot".to_string(), Work::Snapshot));
        for rule in core.config.heal.iter().filter(|rule| !matches!(rule.when, When::Failing(_))) {
            self.jobs.push(on_demand(format!("heal:{}", rule.name), Work::Heal(Box::new(rule.clone()), String::new())));
        }
        if core.config.firewall.manage {
            self.jobs.push(on_demand("firewall".to_string(), Work::Firewall));
        }
        // the first start since the machine booted (not a restart of the service) has more to do
        let mut boot = on_demand("boot".to_string(), Work::Boot);
        let booted: Option<String> =
            db.conn().query_row("SELECT value FROM settings WHERE key = 'daemon.boot_id'", [], |r| r.get(0)).ok();
        let this_boot = jobs::boot_id(core.host.as_ref());
        if this_boot.as_ref().is_some_and(|id| Some(id) != booted.as_ref()) {
            boot.next = self.started;
            // from here until it has been verified, this boot is held to what ran before it
            if let Some(id) = &this_boot {
                crate::restore::verify::expect_verification(&db, id);
            }
        }
        self.jobs.push(boot);
        // the verification a boot is owed: some minutes after it, so that everything had time to start
        let mut verify = on_demand("verify".to_string(), Work::Verify);
        if crate::restore::verify::pending(&db).is_some() {
            verify.next = self.started + i64::try_from(core.config.boot.verify_after.as_secs()).unwrap_or(240);
        }
        self.jobs.push(verify);
        Ok(())
    }

    /// Docker came back after a restart: its chains may be new, so the managed ones are put back.
    pub fn docker_back(&mut self) {
        let now = self.now();
        if let Some(job) = self.jobs.iter_mut().find(|j| j.name == "firewall") {
            job.next = now;
        }
    }

    /// Something on the machine changed: a snapshot follows once things are quiet.
    pub fn changed(&mut self) {
        let now = self.now();
        self.changes.note(now);
        let due = self.changes.due_at().unwrap_or(i64::MAX);
        if let Some(job) = self.jobs.iter_mut().find(|j| j.name == "snapshot") {
            job.next = due;
        }
    }

    /// An event from the Docker engine.
    pub fn docker_event(&mut self, event: &crate::docker::Event) {
        if watch::matters(event) {
            self.changed();
        }
        let now = self.now();
        for job in &mut self.jobs {
            if let Work::Heal(rule, why) = &mut job.work
                && let When::DockerEvent { container, action } = &rule.when
                && event.kind == "container"
                && event.action == *action
                && event.name() == Some(container.as_str())
            {
                *why = format!("{container} {action}");
                job.next = now;
            }
        }
    }

    /// A watched file changed.
    pub fn file_changed(&mut self, path: &Path) {
        // moi's own configuration: take it at once
        if path == self.paths.config || path == self.paths.secrets {
            let log = Logger::new(Arc::clone(&self.core.host), "moi").with_masker(self.core.masker.clone());
            match self.reload() {
                Ok(()) => log.info("configuration reloaded", &[]),
                Err(e) => log.warn(&format!("configuration not reloaded, keeping the one in use: {e}"), &[]),
            }
        }
        let now = self.now();
        for job in &mut self.jobs {
            if let Work::Heal(rule, why) = &mut job.work
                && matches!(&rule.when, When::PathChanged(watched) if Path::new(watched) == path)
            {
                *why = format!("{} changed", path.display());
                job.next = now;
            }
        }
        if !watch::noise(path) {
            self.changed();
        }
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
        self.since.remove(&self.jobs[index].name);
        let job = &mut self.jobs[index];
        job.running = false;
        match &job.schedule {
            // from the time it was due, not from when it ended: no drift; ticks it ran through are skipped
            Schedule::Every(interval) => {
                let step = i64::try_from(interval.as_secs()).unwrap_or(i64::MAX).max(1);
                job.next += step * ((now - job.next).div_euclid(step) + 1);
            }
            Schedule::At(calendar) => job.next = calendar.next_after(now, &core.zone).unwrap_or(i64::MAX),
            Schedule::OnDemand => job.next = i64::MAX,
        }
        // changes seen while the snapshot was being taken want another one
        if job.name == "snapshot" {
            job.next = self.changes.due_at().unwrap_or(i64::MAX);
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
            let work = self.start(index);
            let outcome = jobs::run(&self.core, &work);
            self.finish(index, outcome.clone())?;
            ran.push((self.jobs[index].name.clone(), outcome));
        }
        Ok(ran)
    }

    /// Mark a job as running and hand out its work.
    fn start(&mut self, index: usize) -> Work {
        self.jobs[index].running = true;
        let now = self.now();
        self.since.insert(self.jobs[index].name.clone(), (now, false));
        if self.jobs[index].name == "snapshot" {
            // what was seen so far is being acted on; later changes start a new wait
            self.changes.take();
        }
        self.jobs[index].work.clone()
    }

    /// Mark a job as running without running it (what the loop does before it hands the job to a thread).
    pub fn begin(&mut self, name: &str) -> bool {
        match self.jobs.iter().position(|j| j.name == name) {
            Some(index) => {
                self.start(index);
                true
            }
            None => false,
        }
    }

    /// Running jobs that are past their limit. Each is returned once when it passes the limit, and
    /// again (as `stuck`) every time it is asked about once it is past twice the limit.
    pub fn overdue(&mut self) -> Vec<Overdue> {
        let now = self.now();
        let mut out = Vec::new();
        for job in self.jobs.iter().filter(|j| j.running) {
            let (Some(limit), Some((began, said))) = (job.work.limit(&self.core.config), self.since.get_mut(&job.name))
            else {
                continue;
            };
            let running_for = Duration::from_secs(u64::try_from(now - *began).unwrap_or(0));
            let stuck = running_for >= limit * 2;
            if stuck || (running_for >= limit && !*said) {
                *said = true;
                out.push(Overdue { job: job.name.clone(), running_for, limit, stuck });
            }
        }
        out
    }

    /// Make a job due now (`moi status` shows it running; the CLI's triggers end up here).
    pub fn trigger(&mut self, name: &str) -> bool {
        let now = self.now();
        match self.jobs.iter_mut().find(|j| j.name == name) {
            Some(job) => {
                job.next = job.next.min(now);
                if let Work::Heal(_, why) = &mut job.work {
                    *why = "asked for by hand".to_string();
                }
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
                    // a job that waits to be asked for has no next time
                    "next": (j.next != i64::MAX).then(|| time(j.next)),
                    "last": j.last.as_ref().map(|(at, o)| json!({ "at": time(*at), "status": o.status, "message": o.message, "duration_ms": o.duration_ms })),
                })
            })
            .collect();
        json!({ "running": true, "started": time(self.started), "now": time(self.now()), "version": env!("CARGO_PKG_VERSION"), "jobs": jobs })
    }

    /// The line systemd shows for the unit: what comes next.
    fn status_line(&self) -> String {
        let mut next: Vec<(&i64, &str)> =
            self.jobs.iter().filter(|j| !j.running && j.next != i64::MAX).map(|j| (&j.next, j.name.as_str())).collect();
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
        self.watchers(&events, &log);
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
                let work = self.start(index);
                let (core, done, name) = (Arc::clone(&self.core), events.clone(), self.jobs[index].name.clone());
                let spawned = thread::Builder::new().name(format!("moi-{name}")).spawn(move || {
                    let outcome = jobs::run(&core, &work);
                    let _ = done.send(Event::Done(name, outcome));
                });
                if let Err(e) = spawned {
                    self.finish(
                        index,
                        Outcome { status: "failed".into(), message: format!("could not start: {e}"), duration_ms: 0 },
                    )?;
                }
            }
            // a job that does not come back: said once, and at twice its limit the service gives up so
            // that systemd starts a fresh one (a thread cannot be stopped; a process can)
            for late in self.overdue() {
                let minutes = |d: Duration| d.as_secs() / 60;
                if late.stuck {
                    log.error(
                        &format!(
                            "{} has been running for {} minutes (limit {}): restarting the service",
                            late.job,
                            minutes(late.running_for),
                            minutes(late.limit)
                        ),
                        &[("job", &late.job)],
                    );
                    return Err(Error::Usage(format!(
                        "the job {} is stuck: exiting so that systemd restarts moi",
                        late.job
                    )));
                }
                log.warn(
                    &format!(
                        "{} has been running for {} minutes (limit {}); at twice that the service restarts itself",
                        late.job,
                        minutes(late.running_for),
                        minutes(late.limit)
                    ),
                    &[("job", &late.job)],
                );
            }
            *shared.lock().unwrap_or_else(PoisonError::into_inner) = self.status();
            let _ = host.notify(&format!("WATCHDOG=1\nSTATUS={}", self.status_line()));
            // sleep until the next job is due, something happens, or the watchdog wants to hear from us
            let next = self.jobs.iter().filter(|j| !j.running).map(|j| j.next).min().unwrap_or(i64::MAX);
            let until_next = Duration::from_secs(u64::try_from(next.saturating_sub(now)).unwrap_or(0));
            let wait = until_next.min(watchdog.unwrap_or(MAX_SLEEP)).min(MAX_SLEEP);
            match inbox.recv_timeout(wait) {
                Ok(Event::Done(name, outcome)) => {
                    if matches!(outcome.status.as_str(), "failed") {
                        log.warn(&format!("{name} failed: {}", outcome.message), &[("job", &name)]);
                    }
                    // by name: a reload may have reordered the jobs, or removed this one
                    if let Some(index) = self.jobs.iter().position(|j| j.name == name) {
                        self.finish(index, outcome)?;
                    }
                }
                Ok(Event::Docker(event)) => self.docker_event(&event),
                Ok(Event::DockerBack) => self.docker_back(),
                Ok(Event::File(path)) => self.file_changed(&path),
                Ok(Event::Trigger(name)) => {
                    self.trigger(&name);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }
}

impl Daemon {
    /// Start the threads that watch the Docker engine and the file system.
    fn watchers(&self, events: &mpsc::Sender<Event>, log: &Logger) {
        let host = Arc::clone(&self.core.host);
        let docker = crate::docker::Docker::new(Arc::clone(&host));
        // the engine's event stream; when it ends (Docker restarted) it is picked up where it stopped
        {
            let (docker, events) = (docker.clone(), events.clone());
            let _ = thread::Builder::new().name("moi-docker-events".into()).spawn(move || {
                let mut since = None;
                let mut connected_before = false;
                loop {
                    let types = ["container", "network", "volume", "service"].map(str::to_string).to_vec();
                    let filters = std::collections::BTreeMap::from([("type".to_string(), types)]);
                    if let Ok(stream) = docker.events(&filters, since, None) {
                        if connected_before && events.send(Event::DockerBack).is_err() {
                            return;
                        }
                        connected_before = true;
                        for event in stream.flatten() {
                            since = Some(event.time + 1);
                            if events.send(Event::Docker(event)).is_err() {
                                return;
                            }
                        }
                    }
                    // real time on purpose: this paces reconnecting, it is not something to simulate
                    thread::park_timeout(Duration::from_secs(5));
                }
            });
        }
        let dirs = watch::dirs(host.as_ref(), &self.core.config, &docker);
        match host.watch(&dirs) {
            Ok(mut watching) => {
                let events = events.clone();
                let _ = thread::Builder::new().name("moi-files".into()).spawn(move || {
                    while let Ok(path) = watching.next() {
                        // a directory that appeared below a watched one is watched from now on
                        if host.symlink_metadata(&path).is_ok_and(|m| m.kind == host::FileKind::Dir) {
                            let _ = watching.add(&path);
                        }
                        if events.send(Event::File(path)).is_err() {
                            return;
                        }
                    }
                });
            }
            Err(e) => log.info(&format!("not watching files ({e}): changes are picked up by the daily snapshot"), &[]),
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
