//! One backup run: preflight, database dumps, the local archive, retention and the size guard — then
//! the report. A port of `backup.sh`: every guard it had is here, and its log lines are kept.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Serialize;

use super::archive;
use super::plan::{Dump, Excluder, Plan};
use crate::config::Config;
use crate::daemon::zone::Zone;
use crate::docker::{Docker, ExecOptions};
use crate::error::Error;
use crate::host::{self, FileKind, Host, civil_from_days};
use crate::log::Logger;
use crate::redact::Masker;
use crate::snapshot::View;
use crate::state::StateDir;
use crate::state::db::Db;

const LOCK: &str = "/run/moi-backup.lock";
const GB: u64 = 1 << 30;

/// What a run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub ok: bool,
    /// The run's directory, once there is one.
    pub dir: Option<String>,
    pub dumps: Vec<String>,
    pub files: u64,
    pub bytes: u64,
    /// Every log line, as written to the log file.
    pub lines: Vec<String>,
    /// The last error, shortened: what the monitor is told.
    pub error: Option<String>,
}

/// `2026-11-26 12:00:00` in the server's zone.
fn local(zone: &Zone, at: i64) -> (i64, i64, i64, i64, i64, i64) {
    let local = zone.to_local(at);
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let rem = local.rem_euclid(86_400);
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The run's log: the journal, the legacy log file, and the lines themselves for the report.
struct Log<'a> {
    host: &'a Arc<dyn Host>,
    zone: Zone,
    journal: Logger,
    file: Option<PathBuf>,
    lines: Vec<String>,
    last_error: Option<String>,
    echo: &'a mut dyn FnMut(&str),
}

impl Log<'_> {
    /// `[2026-11-26 12:00:00] message` — the format `backup.sh` wrote.
    fn line(&mut self, message: &str) {
        let (y, m, d, h, mi, s) = local(&self.zone, host::unix_seconds(self.host.now()));
        self.raw(&format!("[{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}] {message}"));
        if message.contains("ERROR:") {
            self.journal.error(message, &[("job", "backup")]);
            self.last_error = Some(message.to_string());
        } else {
            self.journal.info(message, &[("job", "backup")]);
        }
    }

    fn raw(&mut self, line: &str) {
        if let Some(file) = &self.file {
            // the log is not worth failing a backup for
            let _ = self.host.append(file, format!("{line}\n").as_bytes(), 0o644);
        }
        (self.echo)(line);
        self.lines.push(line.to_string());
    }
}

/// `1.2G`, the way `du -sh` rounds up to one decimal.
fn human(bytes: u64) -> String {
    let units = ["K", "M", "G", "T"];
    if bytes < 1024 {
        return format!("{bytes}");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{:.1}{}", (value * 10.0).ceil() / 10.0, units[unit])
    } else {
        format!("{:.0}{}", value.ceil(), units[unit])
    }
}

/// Bytes of every file under a directory.
pub fn tree_size(host: &dyn Host, dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        match host.symlink_metadata(&path) {
            Ok(meta) if meta.kind == FileKind::Dir => {
                stack.extend(host.read_dir(&path).unwrap_or_default().into_iter().map(|n| path.join(n)))
            }
            Ok(meta) => total += meta.size,
            Err(_) => {}
        }
    }
    total
}

/// When a run directory was made, from its name (`2026-11-26_0330`, local time).
fn run_time(name: &str, zone: &Zone) -> Option<i64> {
    let b = name.as_bytes();
    if b.len() != 15 || b[4] != b'-' || b[7] != b'-' || b[10] != b'_' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| name.get(range)?.parse::<i64>().ok();
    let (y, m, d, h, mi) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(13..15)?);
    Some(zone.to_utc(host::days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60))
}

/// Counts what passes through to a writer.
struct Counting<W> {
    inner: W,
    bytes: u64,
}

impl<W: std::io::Write> std::io::Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Dump one database through `docker exec`, compressing as it streams.
fn dump(host: &dyn Host, docker: &Docker, job: &Dump, target: &Path) -> Result<u64, String> {
    let container = job.container.as_deref().ok_or("its container is not running")?;
    let command: Vec<String> = if job.engine == "postgres" {
        ["pg_dumpall", "-h", "127.0.0.1", "-U", job.user.as_str()].map(str::to_string).to_vec()
    } else {
        // MariaDB's client where the image has it, MySQL's otherwise
        let has_mariadb = docker
            .exec(container, &["sh", "-c", "command -v mariadb-dump"], &ExecOptions::default())
            .is_ok_and(|out| out.success());
        [
            if has_mariadb { "mariadb-dump" } else { "mysqldump" },
            "-u",
            "root",
            "--all-databases",
            "--single-transaction",
        ]
        .map(str::to_string)
        .to_vec()
    };
    let words: Vec<&str> = command.iter().map(String::as_str).collect();
    let file = host.create(target, 0o600).map_err(|e| e.to_string())?;
    let mut gz = GzEncoder::new(file, Compression::default());
    let options = ExecOptions { env: job.env.clone(), ..ExecOptions::default() };
    // dumps may run long: no timeout on the stream, the engine's own limits apply
    let mut counted = Counting { inner: &mut gz, bytes: 0 };
    let (exit, stderr) = docker
        .clone()
        .with_timeout(Duration::from_secs(60))
        .exec_to(container, &words, &options, &mut counted)
        .map_err(|e| e.to_string())?;
    let written = counted.bytes;
    gz.finish().and_then(|mut f| std::io::Write::flush(&mut f)).map_err(|e| e.to_string())?;
    if exit != 0 {
        let said = String::from_utf8_lossy(&stderr);
        return Err(format!("exit {exit}: {}", said.lines().last().unwrap_or("no message").trim()));
    }
    if written == 0 {
        return Err("the dump is empty".to_string());
    }
    Ok(written)
}

/// Ask Redis-like stores to write their data to disk and wait until they have, so the files read are
/// a finished save. Best effort: a store that asks for a password is read as it lies.
fn save_redis(host: &dyn Host, docker: &Docker) -> Vec<String> {
    let mut notes = Vec::new();
    for container in docker.containers_inspect().unwrap_or_default() {
        let image = container["Config"]["Image"].as_str().unwrap_or_default();
        let base = image
            .split('@')
            .next()
            .unwrap_or_default()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .split(':')
            .next()
            .unwrap_or_default();
        let persistent = container["Mounts"].as_array().is_some_and(|m| m.iter().any(|m| m["Destination"] == "/data"));
        if !["redis", "valkey", "keydb"].contains(&base) || !persistent || container["State"]["Running"] != true {
            continue;
        }
        let Some(id) = container["Id"].as_str() else { continue };
        let name = container["Name"].as_str().unwrap_or_default().trim_start_matches('/');
        let ask = |command: &[&str]| {
            docker
                .exec(id, &[&["redis-cli"], command].concat(), &ExecOptions::default())
                .ok()
                .filter(|o| o.success())
                .map(|o| o.stdout_str().into_owned())
        };
        // a save in the background, then wait until the store says it is no longer saving
        if ask(&["BGSAVE"]).is_none() {
            notes.push(format!("{name}: could not ask for a save; its files are copied as they lie"));
            continue;
        }
        let saving = || ask(&["INFO", "persistence"]).is_none_or(|info| info.contains("rdb_bgsave_in_progress:1"));
        let mut waited = 0;
        while saving() && waited < 30 {
            host.sleep(Duration::from_secs(1));
            waited += 1;
        }
    }
    notes
}

/// Run a backup. `echo` gets every log line as it is written.
#[allow(clippy::too_many_arguments)]
pub fn run(
    host: &Arc<dyn Host>,
    config: &Arc<Config>,
    state: &StateDir,
    db: &Db,
    masker: &Masker,
    echo: &mut dyn FnMut(&str),
) -> Result<Report, Error> {
    let backup = &config.backup;
    let (Some(local_dir), true) = (&backup.local_dir, backup.enabled) else {
        return Err(Error::Usage("backups are not configured: add a [backup] section with local_dir to moi.toml (`moi backup --dry-run` shows what would be covered)".to_string()));
    };
    let started = host::unix_seconds(host.now());
    let zone = Zone::load(host.as_ref());
    let mut log = Log {
        host,
        zone: zone.clone(),
        journal: Logger::new(Arc::clone(host), "moi").with_masker(masker.clone()),
        file: backup.log_file.as_ref().map(PathBuf::from),
        lines: Vec::new(),
        last_error: None,
        echo,
    };
    let mut report = Report::default();
    let mut fail = false;

    // ---- preflight: one run at a time, and never onto the wrong disk
    let Some(_lock) =
        host.lock(Path::new(LOCK)).map_err(|e| Error::host(format!("cannot take the backup lock {LOCK}"), e))?
    else {
        return Err(Error::Usage("another backup is running".to_string()));
    };
    let mounted = |mount: &String| crate::snapshot::mounts(host.as_ref()).iter().any(|m| m.target == *mount);
    if let Some(mount) = backup.require_mount.as_ref().filter(|m| !mounted(m)) {
        log.line(&format!("ERROR: {mount} not mounted, aborting"));
        return finish(host, config, state, db, masker, report, log, started, true);
    }
    let (y, m, d, h, mi, _) = local(&zone, started);
    let out = Path::new(local_dir).join(format!("{y:04}-{m:02}-{d:02}_{h:02}{mi:02}"));
    host.create_dir_all(&out.join("db"), 0o700)
        .map_err(|e| Error::host(format!("cannot create {}", out.display()), e))?;
    report.dir = Some(out.to_string_lossy().into_owned());

    // ---- what to back up, as the machine is now
    crate::snapshot::capture(host, config, state, db, masker)?;
    let docker = Docker::new(Arc::clone(host));
    let view = View::new(host.as_ref(), state);
    let (plan, identity) = Plan::make(host.as_ref(), config, &docker, &view);
    let excluder = Excluder::new(config, &identity);

    // ---- databases
    for job in &plan.dumps {
        if job.engine == "postgres" {
            log.line(&format!("Dumping Postgres: {} (user {})", job.name, job.user));
        } else {
            log.line(&format!("Dumping MySQL/MariaDB: {}", job.name));
        }
        match dump(host.as_ref(), &docker, job, &out.join("db").join(format!("{}.sql.gz", job.name))) {
            Ok(_) => report.dumps.push(job.name.clone()),
            Err(why) => {
                log.line(&format!("ERROR: dump failed for {} ({why})", job.name));
                fail = true;
            }
        }
    }
    // raw database folders (already dumped), for the off-site step to skip
    let raw = Excluder::raw_database_dirs(&identity);
    let _ = host.write(
        &out.join("raw-db-dirs"),
        raw.iter().map(|d| format!("{d}\n")).collect::<String>().as_bytes(),
        0o600,
    );
    for note in save_redis(host.as_ref(), &docker) {
        log.line(&note);
    }

    // ---- files
    let roots: Vec<String> = plan.roots.iter().map(|r| r.path.clone()).collect();
    log.line(&format!("Archiving configs: {}", roots.join(" ")));
    let staging = state.root().join("staging/sqlite");
    let _ = crate::init::remove_tree(host.as_ref(), &staging);
    host.create_dir_all(&staging, 0o700).map_err(|e| Error::host(format!("cannot create {}", staging.display()), e))?;
    let mut live_databases: Vec<String> = Vec::new();
    match archive::write(host.as_ref(), &roots, &excluder, &out.join("configs.tar.gz"), &staging) {
        Ok(archived) => {
            report.files = archived.files;
            live_databases = archived.databases.clone();
            // what tar would warn about and carry on
            for warning in archived.warnings.iter().take(20) {
                log.line(&format!("warning: {warning}"));
            }
        }
        Err(e) => {
            log.line(&format!("ERROR: tar failed ({e})"));
            fail = true;
        }
    }
    // ---- retention: runs older than keep_local go, by the time in their name
    let limit = started - i64::try_from(backup.keep_local.as_secs()).unwrap_or(i64::MAX);
    for name in host.read_dir(Path::new(local_dir)).unwrap_or_default() {
        if run_time(&name, &zone).is_some_and(|at| at < limit) {
            let _ = crate::init::remove_tree(host.as_ref(), &Path::new(local_dir).join(&name));
        }
    }
    report.bytes = tree_size(host.as_ref(), &out);
    log.raw(&format!("Backup size: {}\t{}", human(report.bytes), out.display()));
    let total = tree_size(host.as_ref(), Path::new(local_dir));
    // whole gigabytes as the script printed them; a smaller limit in its own unit
    let limit = backup.max_local.bytes();
    let (used, allowed) = if limit.is_multiple_of(GB) {
        (format!("{} GB", total / GB), format!("{} GB", limit / GB))
    } else {
        (crate::audit::human(total), crate::audit::human(limit))
    };
    log.line(&format!("All backups: {used} (limit {allowed})"));
    if total > limit {
        log.line(&format!("ERROR: backups exceed {allowed}"));
        fail = true;
    }
    // ---- off-site: only a local backup that is whole goes anywhere
    if !fail && backup.offsite.enabled {
        let mut paths = vec![out.join("db").to_string_lossy().into_owned()];
        paths.extend(roots.iter().cloned());
        let mut excludes: Vec<String> = backup.exclude.clone();
        excludes.extend(raw.iter().cloned());
        excludes.extend(identity.fungible().iter().map(|(path, _)| path.clone()));
        // the tarballs are the local copy; the consistent copies of live databases stand in for the files
        excludes.push(format!("{}/*/configs.tar.gz", local_dir.trim_end_matches('/')));
        for database in &live_databases {
            excludes.extend([
                database.clone(),
                format!("{database}-wal"),
                format!("{database}-shm"),
                format!("{database}-journal"),
            ]);
        }
        let upload = super::offsite::Upload { paths: super::offsite::existing(host.as_ref(), paths), excludes };
        if super::offsite::run(host, config, state, db, &upload, &mut |line| log.line(&format!("offsite: {line}")))
            .is_err()
        {
            log.line("ERROR: off-site backup failed");
            fail = true;
        }
    }
    let _ = crate::init::remove_tree(host.as_ref(), &state.root().join("staging"));
    finish(host, config, state, db, masker, report, log, started, fail)
}

/// The end of every run: say how it went — to the log, the monitor and `moi.db` — and refresh the audit.
#[allow(clippy::too_many_arguments)]
fn finish(
    host: &Arc<dyn Host>,
    config: &Arc<Config>,
    state: &StateDir,
    db: &Db,
    masker: &Masker,
    mut report: Report,
    mut log: Log<'_>,
    started: i64,
    fail: bool,
) -> Result<Report, Error> {
    let dir = report.dir.clone().unwrap_or_default();
    if fail {
        log.line(format!("Backup finished WITH ERRORS: {dir}").trim_end());
    } else {
        log.line(&format!("Backup OK: {dir}"));
    }
    report.ok = !fail;
    // the monitor hears the last error, shortened as the script shortened it
    report.error =
        fail.then(|| log.last_error.clone().unwrap_or_else(|| "backup failed".to_string()).chars().take(180).collect());
    let (status, message) =
        if fail { ("down", report.error.clone().unwrap_or_default()) } else { ("up", "OK".to_string()) };
    let _ = crate::monitors::push(host, config, db, "check:backup", status, &message);
    let now = host::unix_seconds(host.now());
    db.conn()
        .execute(
            "INSERT INTO backup_runs (at, status, bytes, duration_s, message) VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                now,
                if fail { "failed" } else { "ok" },
                i64::try_from(report.bytes).unwrap_or(i64::MAX),
                now - started,
                if fail { message } else { dir },
            ),
        )
        .map_err(|e| Error::Db(e.to_string()))?;
    // what is covered may have changed with what exists
    if host.exists(&state.snapshot()) {
        let _ = crate::audit::run(host, config, state);
        let _ = crate::context::write(host.as_ref(), config, state, masker);
    }
    report.lines = std::mem::take(&mut log.lines);
    Ok(report)
}
