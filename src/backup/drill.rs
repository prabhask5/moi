//! The restore drill: proof that the latest backup can be used, not just that it exists. Each dump is
//! loaded into a throwaway container of the database's own image — no network, removed afterwards —
//! and must come up with tables in it; a sample of the archive's files is compared with the disk.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use flate2::read::GzDecoder;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::plan::{Dump, Plan};
use crate::config::Config;
use crate::docker::{Docker, ExecOptions};
use crate::error::Error;
use crate::host::Host;
use crate::snapshot::View;
use crate::state::StateDir;
use crate::state::db::Db;

/// Files compared with what is on disk.
const SAMPLE: usize = 25;
/// How long a fresh database may take to accept connections.
const STARTUP: u64 = 90;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub ok: bool,
    /// The run that was drilled.
    pub dir: String,
    /// One line per thing tried.
    pub lines: Vec<String>,
}

/// The newest run directory under `local_dir`.
pub fn latest_run(host: &dyn Host, local_dir: &str) -> Option<String> {
    let mut runs: Vec<String> = host
        .read_dir(Path::new(local_dir))
        .ok()?
        .into_iter()
        .filter(|n| n.len() == 15 && n.as_bytes()[10] == b'_')
        .collect();
    runs.sort();
    runs.pop().map(|name| format!("{}/{name}", local_dir.trim_end_matches('/')))
}

/// Load one dump into a fresh container of the same image and count what came out.
fn restore_dump(host: &dyn Host, docker: &Docker, job: &Dump, image: &str, dumps: &Path) -> Result<String, String> {
    let name = format!("moi-drill-{}", job.name);
    let file = format!("/moi-drill/{}.sql.gz", job.name);
    // the dumps as the container sees them: read-only, and nothing else of the host
    let source = host.real_path(dumps).map_err(|e| e.to_string())?;
    let postgres = job.engine == "postgres";
    let password = "moi-drill";
    let env = if postgres {
        vec![format!("POSTGRES_PASSWORD={password}"), format!("POSTGRES_USER={}", job.user)]
    } else {
        vec![format!("MARIADB_ROOT_PASSWORD={password}"), format!("MYSQL_ROOT_PASSWORD={password}")]
    };
    let spec = json!({
        "Image": image,
        "Env": env,
        "Labels": { "moi.drill": job.name },
        "NetworkDisabled": true,
        "HostConfig": { "Binds": [format!("{}:/moi-drill:ro", source.display())], "AutoRemove": false },
    });
    // one left over from a drill that was interrupted
    let _ = docker.remove_container(&name);
    docker.create_container(&name, &spec).map_err(|e| format!("cannot create a container from {image}: {e}"))?;
    let outcome = (|| {
        docker.start(&name).map_err(|e| format!("cannot start it: {e}"))?;
        let sh = |script: &str| {
            docker.clone().with_timeout(Duration::from_secs(1800)).exec(
                &name,
                &["sh", "-c", script],
                &ExecOptions::default(),
            )
        };
        let ready = if postgres {
            format!("pg_isready -q -U {}", job.user)
        } else {
            format!("mysqladmin ping --silent -uroot -p{password}")
        };
        let mut waited = 0;
        while !sh(&ready).is_ok_and(|out| out.success()) {
            if waited >= STARTUP {
                return Err(format!("a fresh {image} did not accept connections within {STARTUP}s"));
            }
            host.sleep(Duration::from_secs(2));
            waited += 2;
        }
        // a cluster dump recreates roles that exist already: such errors are expected, so the measure
        // is what is there afterwards, not the exit code
        let (load, count) = if postgres {
            (
                format!("gunzip -c {file} | psql -q -U {} -d postgres >/dev/null 2>&1; true", job.user),
                format!(
                    "for d in $(psql -U {u} -d postgres -Atc \"SELECT datname FROM pg_database WHERE NOT datistemplate\"); do psql -U {u} -d \"$d\" -Atc \"SELECT count(*) FROM pg_tables WHERE schemaname NOT IN ('pg_catalog','information_schema')\"; done",
                    u = job.user
                ),
            )
        } else {
            (
                format!("gunzip -c {file} | mysql -uroot -p{password} >/dev/null 2>&1; true"),
                format!(
                    "mysql -uroot -p{password} -N -e \"SELECT count(*) FROM information_schema.tables WHERE table_schema NOT IN ('mysql','sys','information_schema','performance_schema')\""
                ),
            )
        };
        let loaded = sh(&load).map_err(|e| format!("cannot load the dump: {e}"))?;
        if !loaded.success() {
            return Err("the dump could not be read".to_string());
        }
        let counted = sh(&count).map_err(|e| format!("cannot look into the restored database: {e}"))?;
        let tables: u64 = counted.stdout_str().lines().filter_map(|l| l.trim().parse::<u64>().ok()).sum();
        if tables == 0 {
            Err("restored, and there is not a single table in it".to_string())
        } else {
            Ok(format!("{tables} tables"))
        }
    })();
    let _ = docker.remove_container(&name);
    outcome
}

/// Compare a sample of the archive's files with the disk. Returns (sampled, identical).
fn sample_files(host: &dyn Host, archive: &Path) -> Result<(usize, usize), String> {
    let reader = host.open(archive).map_err(|e| e.to_string())?;
    let mut tar = tar::Archive::new(GzDecoder::new(reader));
    let (mut seen, mut sampled, mut identical) = (0usize, 0usize, 0usize);
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| format!("the archive is damaged: {e}"))?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        seen += 1;
        // every seventh file, so the sample spreads over the archive (reading the rest checks it is whole)
        if seen % 7 != 1 || sampled >= SAMPLE {
            std::io::copy(&mut entry, &mut std::io::sink()).map_err(|e| format!("the archive is damaged: {e}"))?;
            continue;
        }
        let path = Path::new("/").join(entry.path().map_err(|e| e.to_string())?);
        // hash a stream, piece by piece
        let digest = |reader: &mut dyn Read| -> std::io::Result<Vec<u8>> {
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buffer)? {
                    0 => return Ok(hasher.finalize().to_vec()),
                    n => hasher.update(&buffer[..n]),
                }
            }
        };
        let archived = digest(&mut entry).map_err(|e| format!("the archive is damaged: {e}"))?;
        sampled += 1;
        if host.open(&path).ok().and_then(|mut file| digest(&mut file).ok()) == Some(archived) {
            identical += 1;
        }
    }
    if seen == 0 {
        return Err("the archive holds no files".to_string());
    }
    Ok((sampled, identical))
}

/// Drill the latest local backup.
pub fn run(host: &Arc<dyn Host>, config: &Config, state: &StateDir, db: &Db) -> Result<Report, Error> {
    let local_dir = config
        .backup
        .local_dir
        .as_ref()
        .filter(|_| config.backup.enabled)
        .ok_or_else(|| Error::Usage("backups are not configured: there is nothing to drill".to_string()))?;
    let dir = latest_run(host.as_ref(), local_dir)
        .ok_or_else(|| Error::Usage(format!("there is no backup in {local_dir} yet: run `moi backup` first")))?;
    let docker = Docker::new(Arc::clone(host));
    let view = View::new(host.as_ref(), state);
    let (plan, _) = Plan::make(host.as_ref(), config, &docker, &view);
    let mut report = Report { ok: true, dir: dir.clone(), lines: Vec::new() };
    let containers = docker.containers_inspect().unwrap_or_default();
    let dumps = Path::new(&dir).join("db");
    for name in host.read_dir(&dumps).unwrap_or_default() {
        let Some(database) = name.strip_suffix(".sql.gz") else { continue };
        // how to restore it is known from the database it came from
        let job = plan.dumps.iter().find(|d| d.name == database);
        let image = job
            .and_then(|j| j.container.as_ref())
            .and_then(|id| containers.iter().find(|c| c["Id"] == id.as_str()))
            .and_then(|c| c["Config"]["Image"].as_str());
        match (job, image) {
            (Some(job), Some(image)) => match restore_dump(host.as_ref(), &docker, job, image, &dumps) {
                Ok(found) => report.lines.push(format!("{database}: restored into a fresh {image}: {found}")),
                Err(why) => {
                    report.ok = false;
                    report.lines.push(format!("{database}: FAILED: {why}"));
                }
            },
            _ => report.lines.push(format!(
                "{database}: skipped (its database is not running, so the image to restore into is unknown)"
            )),
        }
    }
    match sample_files(host.as_ref(), &Path::new(&dir).join("configs.tar.gz")) {
        Ok((sampled, identical)) => {
            let changed = sampled - identical;
            let note = if changed == 0 { String::new() } else { format!(", {changed} changed since the backup") };
            report.lines.push(format!(
                "files: archive read to its end; {sampled} sampled, {identical} identical to the disk{note}"
            ));
        }
        Err(why) => {
            report.ok = false;
            report.lines.push(format!("files: FAILED: {why}"));
        }
    }
    // the monitor hears how it went, when there is one
    let failed: Vec<&String> = report.lines.iter().filter(|l| l.contains("FAILED")).collect();
    let message = if report.ok {
        format!("drill OK: {} checks on {dir}", report.lines.len())
    } else {
        failed.first().map(|l| l.chars().take(180).collect()).unwrap_or_default()
    };
    let _ = crate::monitors::push(host, config, db, "backup:drill", if report.ok { "up" } else { "down" }, &message);
    Ok(report)
}
