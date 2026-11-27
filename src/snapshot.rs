//! Snapshot: collectors capture the server's state into `state/snapshot/` (stable, versioned) and
//! `state/volatile/` (changes on its own, git-ignored).
//!
//! Spec: docs/spec/02-snapshot.md. One collector per area. Collectors only *describe* what to store —
//! generated files as bytes, host files as paths — and the engine here does all writing: it rewrites
//! only what changed, copies host files through the `(size, mtime, inode) → sha256` cache in `moi.db`,
//! removes what is no longer produced, and keeps the previous output of a collector that failed or
//! timed out.
//!
//! Concurrency: plain threads, one per collector, joined through a channel with a deadline. Collectors
//! are a dozen blocking jobs dominated by file and socket I/O; a thread each is the whole scheduling
//! problem, so neither rayon nor an async runtime is needed here.

mod collect;
mod cron;
mod docker;
mod etc;
mod files;
mod firewall;
mod incus;
mod meta;
mod monitoring;
mod network;
mod packages;
mod scripts;
mod storage;
mod systemd;
mod trees;
mod users;
mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

pub use collect::{Collector, Ctx, Output, Walk, run_tool, walk_files};
pub use view::{CronJob, View, parse_crontab};

use crate::config::Config;
use crate::docker::Docker;
use crate::error::Error;
use crate::host::{self, FileKind, Host};
use crate::state::StateDir;
use crate::state::db::Db;

/// How a snapshot run is bounded.
#[derive(Debug, Clone)]
pub struct Options {
    /// Longest a single collector may take before it is given up on (its previous output is kept).
    pub timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self { timeout: Duration::from_secs(60) }
    }
}

/// What one collector did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CollectorReport {
    pub name: String,
    /// `None` if it succeeded; otherwise why its previous output was kept.
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// What a snapshot run did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub collectors: Vec<CollectorReport>,
    /// Paths under `state/snapshot/` that were written or removed (relative to `state/`), sorted.
    pub changed: Vec<String>,
    /// Number of files now in `state/snapshot/`.
    pub stable_files: usize,
    /// Host files that were read (not served from the cache).
    pub files_read: usize,
}

impl Report {
    /// Collectors whose previous output was kept.
    pub fn failed(&self) -> impl Iterator<Item = &CollectorReport> {
        self.collectors.iter().filter(|c| c.error.is_some())
    }
}

/// The built-in collectors, in the order their output is applied (earlier wins a shared file).
pub fn collectors() -> Vec<Arc<dyn Collector>> {
    vec![
        Arc::new(meta::Meta),
        Arc::new(packages::Packages),
        Arc::new(etc::Etc),
        Arc::new(users::Users),
        Arc::new(storage::Storage),
        Arc::new(trees::Trees),
        Arc::new(network::Network),
        Arc::new(firewall::Firewall),
        Arc::new(systemd::Systemd),
        Arc::new(cron::Cron),
        Arc::new(docker::Docker),
        Arc::new(scripts::Scripts),
        Arc::new(incus::Incus),
        Arc::new(monitoring::Monitoring),
    ]
}

/// What `moi snapshot` and the daemon do: capture, add note stubs for new things, commit if something
/// changed. Returns the capture report and the commit, if one was made.
pub fn capture(
    host: &Arc<dyn Host>,
    config: &Arc<Config>,
    state: &StateDir,
    db: &Db,
) -> Result<(Report, Option<crate::state::history::Entry>), Error> {
    let report = run(host, config, state, db)?;
    crate::state::notes::update(host.as_ref(), state)?;
    let commit = crate::state::history::record(host.as_ref(), state)?;
    Ok((report, commit))
}

/// Capture the host's state with the built-in collectors.
pub fn run(host: &Arc<dyn Host>, config: &Arc<Config>, state: &StateDir, db: &Db) -> Result<Report, Error> {
    run_with(host, config, state, db, &collectors(), &Options::default())
}

/// Capture with an explicit collector list and options.
pub fn run_with(
    host: &Arc<dyn Host>,
    config: &Arc<Config>,
    state: &StateDir,
    db: &Db,
    collectors: &[Arc<dyn Collector>],
    options: &Options,
) -> Result<Report, Error> {
    state.ensure(host.as_ref())?;
    // ---- collect: one round concurrently, then the collectors that need what the first round captured
    let mut results: Vec<Option<(Result<Output, String>, Duration)>> = (0..collectors.len()).map(|_| None).collect();
    let mut cx = Ctx {
        host: Arc::clone(host),
        config: Arc::clone(config),
        docker: Docker::new(Arc::clone(host)),
        captured: Vec::new(),
        generated: BTreeMap::new(),
        failed: Vec::new(),
    };
    for second_round in [false, true] {
        if second_round {
            for (collector, result) in collectors.iter().zip(&results) {
                match result {
                    Some((Ok(output), _)) => {
                        cx.captured.extend(output.files.iter().cloned());
                        cx.generated.extend(output.stable.iter().cloned());
                    }
                    _ if !collector.after_capture() => cx.failed.push(collector.name().to_string()),
                    _ => {}
                }
            }
            cx.captured.sort();
            cx.captured.dedup();
        }
        let round = Arc::new(Ctx {
            host: Arc::clone(&cx.host),
            config: Arc::clone(&cx.config),
            docker: cx.docker.clone(),
            captured: cx.captured.clone(),
            generated: cx.generated.clone(),
            failed: cx.failed.clone(),
        });
        let (tx, rx) = mpsc::channel();
        let mut pending = 0;
        for (index, collector) in collectors.iter().enumerate().filter(|(_, c)| c.after_capture() == second_round) {
            pending += 1;
            let (thread_tx, round, collector) = (tx.clone(), Arc::clone(&round), Arc::clone(collector));
            let spawned = thread::Builder::new().name(format!("moi-{}", collector.name())).spawn(move || {
                let started = Instant::now();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| collector.collect(&round)))
                    .unwrap_or_else(|_| Err("collector panicked".to_string()));
                // The receiver is gone if this collector outlived its deadline; nothing to do then.
                let _ = thread_tx.send((index, result, started.elapsed()));
            });
            if let Err(e) = spawned {
                let _ = tx.send((index, Err(format!("could not start: {e}")), Duration::ZERO));
            }
        }
        drop(tx);
        let deadline = Instant::now() + options.timeout;
        while pending > 0 {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((index, result, took)) => {
                    results[index] = Some((result, took));
                    pending -= 1;
                }
                // Deadline passed (or every thread is gone). A collector still running is abandoned: it
                // only reads, and whatever it sends later is dropped.
                Err(_) => break,
            }
        }
    }

    // ---- apply
    let store = files::Store::open(host.as_ref(), state, db, config.capture.max_file.bytes())?;
    let mut store = store;
    let mut reports = Vec::new();
    for (collector, result) in collectors.iter().zip(results) {
        let name = collector.name();
        let (result, took) =
            result.unwrap_or_else(|| (Err(format!("timed out after {}s", options.timeout.as_secs())), options.timeout));
        let error = match result {
            Ok(output) => {
                store.apply(name, output)?;
                None
            }
            Err(e) => {
                store.keep_previous(name);
                Some(e)
            }
        };
        reports.push(CollectorReport {
            name: name.to_string(),
            error,
            duration_ms: u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
        });
    }

    // ---- what the engine itself writes
    let mut own = Output::default();
    own.volatile("captured-at", format!("{}\n", host::format_rfc3339(host.now())));
    let failed: Vec<&CollectorReport> = reports.iter().filter(|r| r.error.is_some()).collect();
    if !failed.is_empty() {
        let stale: Vec<_> = failed
            .iter()
            .map(|r| serde_json::json!({ "collector": r.name, "error": r.error, "stale": true }))
            .collect();
        own.volatile(
            "collector-errors.json",
            format!("{}\n", serde_json::to_string_pretty(&stale).unwrap_or_default()),
        );
    }
    own.stable("files.manifest.tsv", store.manifest());
    store.apply("snapshot", own)?;

    let outcome = store.finish()?;
    Ok(Report {
        collectors: reports,
        changed: outcome.changed,
        stable_files: outcome.stable_files,
        files_read: outcome.files_read,
    })
}

/// Every file under a state subdirectory, relative to `state/` (`snapshot/meta/hostname`), sorted.
pub fn list_tree(host: &dyn Host, state_root: &Path, top: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    let mut stack: Vec<PathBuf> = vec![PathBuf::from(top)];
    while let Some(rel) = stack.pop() {
        let Ok(names) = host.read_dir(&state_root.join(&rel)) else { continue };
        for name in names {
            let child = rel.join(&name);
            match host.symlink_metadata(&state_root.join(&child)).map(|m| m.kind) {
                Ok(FileKind::Dir) => stack.push(child),
                Ok(_) => {
                    out.insert(child.to_string_lossy().into_owned());
                }
                Err(_) => {}
            }
        }
    }
    out.into_iter().collect()
}

/// Names by numeric id from a passwd- or group-format file (`name:x:id:…`).
pub(crate) fn id_names(host: &dyn Host, file: &str) -> BTreeMap<u32, String> {
    let mut map = BTreeMap::new();
    for line in host.read_to_string(Path::new(file)).unwrap_or_default().lines() {
        let mut fields = line.split(':');
        if let (Some(name), Some(_), Some(id)) = (fields.next(), fields.next(), fields.next())
            && let Ok(id) = id.parse()
        {
            map.entry(id).or_insert_with(|| name.to_string());
        }
    }
    map
}
