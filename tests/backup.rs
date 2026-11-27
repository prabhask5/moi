//! `moi backup`: the plan, the local run and its guards (docs/spec/07-backup.md).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use cestmoi::cli;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::state::StateDir;

/// An archive entry: path, kind (`d`, `l`, `f`), mode, size, link target.
type Entry = (String, char, u32, u64, String);

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
}

fn server(test: &str) -> Server {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    Server { host: fixture.clone(), fixture }
}

/// The run directory of a backup started at the fixture's clock: noon in New York.
const RUN: &str = "/mnt/storage/backups/2026-11-26_1200";

impl Server {
    fn moi(&self, args: &[&str]) -> (u8, String) {
        let mut out = Vec::new();
        match cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out) {
            Ok(()) => (0, String::from_utf8(out).unwrap()),
            Err(e) if e.exit_code() == 1 => (1, String::from_utf8(out).unwrap()),
            Err(e) => (e.exit_code(), e.to_string()),
        }
    }

    fn edit_config(&self, edit: impl FnOnce(String) -> String) {
        let path = Path::new("/etc/moi/moi.toml");
        let text = edit(self.host.read_to_string(path).unwrap());
        self.host.write(path, text.as_bytes(), 0o644).unwrap();
    }

    /// Pushes Kuma received, as the raw query string.
    fn kuma(&self) -> Arc<Mutex<Vec<String>>> {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        self.fixture.set_http_at(
            "uptime.home:80",
            Arc::new(move |_, raw| {
                let line = String::from_utf8_lossy(raw).lines().next().unwrap_or_default().to_string();
                if let Some(rest) = line.strip_prefix("GET /api/push/") {
                    log.lock().unwrap().push(rest.trim_end_matches(" HTTP/1.1").to_string());
                }
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}".to_vec()
            }),
        );
        StateDir::new("/var/lib/moi").ensure(self.host.as_ref()).unwrap();
        let db = StateDir::new("/var/lib/moi").open_db(self.host.as_ref()).unwrap();
        db.conn().execute("INSERT INTO monitor_bindings (owner, monitor_name, push_token, updated_at) VALUES ('check:backup', 'Server Backup', 'FAKEbackupTok00004', 0)", []).unwrap();
        seen
    }

    fn gunzip(&self, path: &str) -> Vec<u8> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(self.host.open(Path::new(path)).unwrap()).read_to_end(&mut out).unwrap();
        out
    }

    /// (path, kind, mode, size, link target) of every entry in the archive, and the content of `wanted`.
    fn archive(&self, wanted: &str) -> (Vec<Entry>, Vec<u8>) {
        let raw = self.gunzip(&format!("{RUN}/configs.tar.gz"));
        let mut archive = tar::Archive::new(raw.as_slice());
        let (mut entries, mut content) = (Vec::new(), Vec::new());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let header = entry.header().clone();
            let kind = match header.entry_type() {
                tar::EntryType::Directory => 'd',
                tar::EntryType::Symlink => 'l',
                _ => 'f',
            };
            let link = header.link_name().unwrap().map(|l| l.to_string_lossy().into_owned()).unwrap_or_default();
            if path == wanted {
                entry.read_to_end(&mut content).unwrap();
            }
            entries.push((path, kind, header.mode().unwrap(), header.size().unwrap(), link));
        }
        (entries, content)
    }
}

#[test]
fn dry_run_shows_what_would_be_backed_up_and_writes_nothing() {
    let s = server("backup-dry-run");
    let (code, out) = s.moi(&["backup", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    common::assert_golden(
        &common::fixture_dir("cestmoi").parent().unwrap().parent().unwrap().join("expected/cestmoi/backup-dry-run.txt"),
        &out,
    );
    // the databases, each with the user it is dumped as — the swarm service under its stable name
    for line in [
        "  outline-postgres             postgres  user outline",
        "  bookorbit-db                 postgres  user bookorbit",
        "  supabase-db                  postgres  user supabase_admin",
        "  dokploy-postgres             postgres  user dokploy",
    ] {
        assert!(out.contains(&format!("{line}\n")), "missing {line:?}");
    }
    // volumes of swarm services, and paths from the configuration
    for path in [
        "/var/lib/docker/volumes/stellar-data/_data",
        "/var/lib/docker/volumes/shortless-redis-m3n4-data/_data",
        "/var/lib/docker/volumes/dokploy-docker-config/_data",
        "/opt/services",
        "/opt/yams",
        "/etc/moi",
    ] {
        assert!(out.lines().any(|l| l.starts_with(&format!("  {path} "))), "{path} should be in the set:\n{out}");
    }
    // never the fungible, never raw database files
    let left_out = out.split("left out:\n").nth(1).unwrap();
    for (path, why) in [
        ("/mnt/storage/media", "fungible: media library"),
        ("/var/lib/incus", "fungible: devbox VM"),
        ("/opt/services/outline/pgdata", "raw files of outline-postgres (dumped instead)"),
        ("/mnt/storage/database/db-data", "raw files of supabase-db (dumped instead)"),
        ("/var/lib/docker/volumes/dokploy-postgres-database/_data", "raw files of dokploy-postgres (dumped instead)"),
    ] {
        assert!(
            left_out.lines().any(|l| l.trim_start().starts_with(path) && l.contains(why)),
            "{path}: {why}\n{left_out}"
        );
    }
    assert!(
        out.ends_with("guards:\n  ok   /mnt/storage is mounted\n  ok   local backups use 0 B of 20.0 GB allowed\n"),
        "{out}"
    );
    // nothing was written where backups go, no dump was run, no password is shown
    assert_eq!(s.host.read_dir(Path::new("/mnt/storage/backups")).unwrap(), Vec::<String>::new());
    assert!(s.fixture.docker_requests().iter().all(|r| r.starts_with("GET ")), "{:?}", s.fixture.docker_requests());
    let secrets = std::fs::read_to_string(common::fixture_dir("cestmoi").join("secrets")).unwrap();
    let json = s.moi(&["backup", "--dry-run", "--json"]).1;
    for secret in secrets.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        assert!(!out.contains(secret) && !json.contains(secret), "a secret is shown: {secret}");
    }
    assert!(!json.contains("PGPASSWORD"));
}

#[test]
fn a_run_dumps_every_database_archives_the_set_and_reports_ok() {
    let s = server("backup-run");
    let pushes = s.kuma();
    s.fixture.set_env("JOURNAL_STREAM", "8:1");
    let (code, out) = s.moi(&["backup"]);
    assert_eq!(code, 0, "{out}");
    // the log, in the legacy format
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "[2026-11-26 12:00:00] Dumping Postgres: bookorbit-db (user bookorbit)");
    assert!(lines.contains(&"[2026-11-26 12:00:00] Dumping Postgres: supabase-db (user supabase_admin)"));
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("[2026-11-26 12:00:00] Archiving configs: /etc/docker /etc/dokploy /etc/moi ")),
        "{out}"
    );
    assert!(lines.iter().any(|l| l.starts_with("Backup size: ") && l.ends_with(&format!("\t{RUN}"))), "{out}");
    assert!(lines.contains(&"[2026-11-26 12:00:00] All backups: 0 GB (limit 20 GB)"));
    assert_eq!(*lines.last().unwrap(), format!("[2026-11-26 12:00:00] Backup OK: {RUN}"));
    assert!(!out.contains("ERROR") && !out.contains("warning"), "{out}");
    // appended to the log file the host-logs container reads, after what the legacy script wrote
    let log = s.host.read_to_string(Path::new("/var/log/server-backup.log")).unwrap();
    assert!(
        log.contains("[2026-11-26 03:41:10] offsite: uploaded\n[2026-11-26 12:00:00] Dumping Postgres: bookorbit-db"),
        "{log}"
    );
    assert!(log.ends_with(&format!("[2026-11-26 12:00:00] Backup OK: {RUN}\n")));

    // dumps: what the database printed, compressed; one per database, the service under its own name
    let mut dumps = s.host.read_dir(Path::new(&format!("{RUN}/db"))).unwrap();
    dumps.sort();
    assert_eq!(
        dumps,
        ["bookorbit-db.sql.gz", "dokploy-postgres.sql.gz", "outline-postgres.sql.gz", "supabase-db.sql.gz"]
    );
    let dump = String::from_utf8(s.gunzip(&format!("{RUN}/db/dokploy-postgres.sql.gz"))).unwrap();
    assert!(
        dump.starts_with("--\n-- PostgreSQL database cluster dump\n")
            && dump.contains("1\tdumped from dokploy-postgres\n"),
        "{dump}"
    );
    // every dump ran inside its container (and Redis was asked to save before its files were read)
    let execs = s
        .fixture
        .docker_requests()
        .into_iter()
        .filter(|r| r.starts_with("POST /containers/") && r.ends_with("/exec"))
        .count();
    assert!(execs >= 4, "{execs}");
    assert_eq!(s.host.read_to_string(Path::new(&format!("{RUN}/raw-db-dirs"))).unwrap().lines().count(), 4);
    // private: dumps hold everything
    assert_eq!(s.host.metadata(Path::new(&format!("{RUN}/db/outline-postgres.sql.gz"))).unwrap().mode, 0o600);
    assert_eq!(s.host.metadata(Path::new(RUN)).unwrap().mode, 0o700);

    // the archive
    let (entries, kuma_db) = s.archive("opt/services/monitoring/uptime-kuma/data/kuma.db");
    let has = |path: &str| entries.iter().any(|(p, ..)| p == path);
    for path in [
        "opt/scripts/resource-check.sh",
        "opt/yams/docker-compose.yml",
        "etc/moi/moi.toml",
        "var/lib/docker/volumes/stellar-data/_data/planner.json",
        "etc/systemd/system/host-firewall.service",
    ] {
        assert!(has(path), "{path} should be archived");
    }
    for (prefix, why) in [
        ("mnt/storage/media", "fungible"),
        ("opt/services/outline/pgdata", "raw database files"),
        ("opt/yams/config/jellyfin/cache", "backup.exclude */cache"),
        ("opt/yams/config/sonarr/MediaCover", "tagged as a cache directory"),
        ("mnt/storage/backups", "the backups themselves"),
        ("var/lib/moi/staging", "staging"),
        ("etc/dokploy/logs", "backup.exclude"),
    ] {
        assert!(!entries.iter().any(|(p, ..)| p.starts_with(prefix)), "{prefix} must not be archived ({why})");
    }
    assert!(has("opt/yams/config/jellyfin") && has("opt/yams/config/sonarr"), "their parents are");
    // a symlink stays a symlink; modes and types are kept
    let link = entries.iter().find(|(p, ..)| p == "opt/yams/current.env").unwrap();
    assert_eq!((link.1, link.4.as_str()), ('l', ".env"));
    let script = entries.iter().find(|(p, ..)| p == "opt/scripts/resource-check.sh").unwrap();
    assert_eq!((script.1, script.2 & 0o111 != 0), ('f', true), "{script:?}");
    assert_eq!(entries.iter().find(|(p, ..)| p == "opt/scripts").unwrap().1, 'd');
    // a live SQLite database is in the archive as a consistent copy that opens
    let copy = common::scratch("backup-run-kuma").join("kuma.db");
    std::fs::write(&copy, &kuma_db).unwrap();
    let monitors: i64 =
        rusqlite::Connection::open(&copy).unwrap().query_row("SELECT count(*) FROM monitor", [], |r| r.get(0)).unwrap();
    assert_eq!(monitors, 21);
    assert!(!entries.iter().any(|(p, ..)| p.ends_with("-wal") || p.ends_with("-shm")));
    assert_eq!(
        s.host.read_dir(Path::new("/var/lib/moi/staging")).unwrap_or_default(),
        Vec::<String>::new(),
        "staging is cleaned up"
    );

    // the monitor hears OK, once, with one status and one message
    let pushed = pushes.lock().unwrap().clone();
    assert_eq!(pushed, ["FAKEbackupTok00004?status=up&msg=OK&ping="]);
    // and the run is on record
    let db = StateDir::new("/var/lib/moi").open_db(s.host.as_ref()).unwrap();
    let (status, bytes, message): (String, i64, String) = db
        .conn()
        .query_row("SELECT status, bytes, message FROM backup_runs", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    assert_eq!((status.as_str(), message.as_str(), bytes > 1000), ("ok", RUN, true));
}

#[test]
fn nothing_is_written_when_the_backup_disk_is_not_mounted() {
    let case = common::scratch("backup-no-mount-case");
    let mountinfo = std::fs::read_to_string(common::fixture_dir("cestmoi").join("root/proc/self/mountinfo")).unwrap();
    std::fs::create_dir_all(case.join("root/proc/self")).unwrap();
    std::fs::write(
        case.join("root/proc/self/mountinfo"),
        mountinfo.lines().filter(|l| !l.contains(" /mnt/storage ")).map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    let overlay = common::scratch("backup-no-mount");
    let fixture = Arc::new(
        FixtureHost::open(&common::fixture_dir("cestmoi"))
            .unwrap()
            .with_case(&case)
            .unwrap()
            .with_overlay(&overlay)
            .unwrap(),
    );
    let s = Server { host: fixture.clone(), fixture };
    let pushes = s.kuma();
    let (code, out) = s.moi(&["backup"]);
    assert_eq!(code, 1);
    assert_eq!(
        out,
        "[2026-11-26 12:00:00] ERROR: /mnt/storage not mounted, aborting\n[2026-11-26 12:00:00] Backup finished WITH ERRORS:\n"
    );
    // the SSD under the mount point stays empty, and no database was touched
    assert_eq!(s.host.read_dir(Path::new("/mnt/storage/backups")).unwrap(), Vec::<String>::new());
    assert!(!s.fixture.docker_requests().iter().any(|r| r.contains("/exec")));
    assert_eq!(
        *pushes.lock().unwrap(),
        ["FAKEbackupTok00004?status=down&msg=ERROR%3A%20%2Fmnt%2Fstorage%20not%20mounted%2C%20aborting&ping="]
    );
    // the dry run says so too
    let (code, out) = s.moi(&["backup", "--dry-run"]);
    assert_eq!(code, 1);
    assert!(out.contains("  FAIL /mnt/storage is not mounted\n"), "{out}");
}

#[test]
fn a_failing_dump_fails_the_run_and_the_monitor_hears_the_last_error() {
    // pg_dumpall in the outline database exits with an error
    let case = common::scratch("backup-dump-fails-case");
    let exec = case.join("docker/exec/outline-postgres");
    std::fs::create_dir_all(&exec).unwrap();
    let key = FixtureHost::command_key("pg_dumpall -h 127.0.0.1 -U outline");
    std::fs::write(exec.join(format!("{key}.out")), "").unwrap();
    std::fs::write(exec.join(format!("{key}.exit")), "2\n").unwrap();
    std::fs::write(
        exec.join(format!("{key}.err")),
        "pg_dumpall: error: connection to server failed: FATAL:  password authentication failed for user \"outline\"\n",
    )
    .unwrap();
    let overlay = common::scratch("backup-dump-fails");
    let fixture = Arc::new(
        FixtureHost::open(&common::fixture_dir("cestmoi"))
            .unwrap()
            .with_case(&case)
            .unwrap()
            .with_overlay(&overlay)
            .unwrap(),
    );
    let s = Server { host: fixture.clone(), fixture };
    let pushes = s.kuma();
    let (code, out) = s.moi(&["backup"]);
    assert_eq!(code, 1);
    let error = "ERROR: dump failed for outline-postgres (exit 2: pg_dumpall: error: connection to server failed: FATAL:  password authentication failed for user \"outline\")";
    assert!(out.contains(&format!("[2026-11-26 12:00:00] {error}\n")), "{out}");
    assert!(out.ends_with(&format!("[2026-11-26 12:00:00] Backup finished WITH ERRORS: {RUN}\n")));
    // the other databases and the files are still backed up
    assert!(
        s.host.exists(Path::new(&format!("{RUN}/db/supabase-db.sql.gz")))
            && s.host.exists(Path::new(&format!("{RUN}/configs.tar.gz")))
    );
    // the monitor gets the last ERROR line, cut at 180 characters, as one message
    let pushed = pushes.lock().unwrap().clone();
    assert_eq!(pushed.len(), 1);
    assert!(
        pushed[0].starts_with("FAKEbackupTok00004?status=down&msg=ERROR%3A%20dump%20failed%20for%20outline-postgres"),
        "{pushed:?}"
    );
    assert_eq!(pushed[0].matches("status=").count(), 1);
    let message: String = error.chars().take(180).collect();
    let db = StateDir::new("/var/lib/moi").open_db(s.host.as_ref()).unwrap();
    assert_eq!(
        db.conn()
            .query_row("SELECT message FROM backup_runs WHERE status = 'failed'", [], |r| r.get::<_, String>(0))
            .unwrap(),
        message
    );
}

#[test]
fn old_runs_are_removed_and_too_much_backup_is_an_error() {
    let s = server("backup-retention");
    let dir = |name: &str| {
        let path = Path::new("/mnt/storage/backups").join(name);
        s.host.create_dir_all(&path, 0o700).unwrap();
        s.host.write(&path.join("configs.tar.gz"), &vec![7u8; 5000], 0o600).unwrap();
    };
    // keep_local = 7d: runs by the time in their name; what is not a run is never touched
    for name in [
        "2026-11-10_0330",
        "2026-11-19_1159",
        "2026-11-19_1201",
        "2026-11-25_0330",
        "notes-from-2019",
        "2026-11-01_0330.keep",
    ] {
        dir(name);
    }
    assert_eq!(s.moi(&["backup"]).0, 0);
    let mut left = s.host.read_dir(Path::new("/mnt/storage/backups")).unwrap();
    left.sort();
    assert_eq!(
        left,
        ["2026-11-01_0330.keep", "2026-11-19_1201", "2026-11-25_0330", "2026-11-26_1200", "notes-from-2019"]
    );

    // more than max_local in total: the run counts as failed
    let s = server("backup-size-guard");
    s.edit_config(|t| t.replace("max_local = \"20GB\"", "max_local = \"1KB\""));
    let (code, out) = s.moi(&["backup"]);
    assert_eq!(code, 1);
    assert!(out.contains("] ERROR: backups exceed 1.0 KB\n"), "{out}");
    assert!(s.moi(&["backup", "--dry-run"]).1.contains("  FAIL local backups use "), "the dry run runs the guard too");
}

#[test]
fn one_backup_at_a_time_and_none_without_a_configuration() {
    let s = server("backup-lock");
    let held = s.host.lock(Path::new("/run/moi-backup.lock")).unwrap().expect("free");
    assert_eq!(s.moi(&["backup"]), (2, "another backup is running".to_string()));
    drop(held);
    assert_eq!(s.moi(&["backup"]).0, 0);

    let overlay = common::scratch("backup-unconfigured");
    let host: Arc<dyn Host> =
        Arc::new(FixtureHost::open(&common::fixture_dir("minimal")).unwrap().with_overlay(&overlay).unwrap());
    let err = cli::execute(&cli::parse(&["backup"]).unwrap(), host.clone(), &mut Vec::new()).unwrap_err();
    assert!(err.to_string().starts_with("backups are not configured: add a [backup] section with local_dir"), "{err}");
    let mut out = Vec::new();
    let dry = cli::execute(&cli::parse(&["backup", "--dry-run"]).unwrap(), host, &mut out);
    assert!(dry.is_err() && String::from_utf8(out).unwrap().contains("FAIL backups are not switched on"));
}

#[test]
fn the_daemon_backs_up_at_the_configured_time() {
    let s = server("backup-daemon");
    let loaded =
        cestmoi::config::load(s.host.as_ref(), &cestmoi::config::Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let mut daemon = cestmoi::daemon::Daemon::new(s.host.clone(), loaded).unwrap();
    let at = |time: &str| cestmoi::host::unix_seconds(cestmoi::host::parse_rfc3339(time).unwrap());
    // 03:30 in New York
    assert_eq!(daemon.jobs().iter().find(|j| j.name == "backup").unwrap().next, at("2026-11-27T08:30:00Z"));
    daemon.tick().unwrap();
    assert!(!s.host.exists(Path::new("/mnt/storage/backups/2026-11-27_0330")));
    s.fixture.set_now(cestmoi::host::parse_rfc3339("2026-11-27T08:30:00Z").unwrap());
    let ran = daemon.tick().unwrap();
    let backup = &ran.iter().find(|(name, _)| name == "backup").expect("the backup ran").1;
    assert_eq!((backup.status.as_str(), backup.message.as_str()), ("ok", "/mnt/storage/backups/2026-11-27_0330"));
    assert!(s.host.exists(Path::new("/mnt/storage/backups/2026-11-27_0330/configs.tar.gz")));
}

// ------------------------------------------------------------------ the restore drill

/// The cestmoi fixture where throwaway containers answer the drill's questions.
fn drill_server(test: &str, tables: &[(&str, &str, &str)]) -> Server {
    let case = common::scratch(&format!("{test}-case"));
    for (database, user, counted) in tables {
        let exec = case.join(format!("docker/exec/moi-drill-{database}"));
        std::fs::create_dir_all(&exec).unwrap();
        let answer = |script: String, out: &str| {
            let key = FixtureHost::command_key(&format!("sh -c {script}"));
            std::fs::write(exec.join(format!("{key}.out")), out).unwrap();
        };
        answer(format!("pg_isready -q -U {user}"), "");
        answer(
            format!("gunzip -c /moi-drill/{database}.sql.gz | psql -q -U {user} -d postgres >/dev/null 2>&1; true"),
            "",
        );
        answer(
            format!(
                "for d in $(psql -U {user} -d postgres -Atc \"SELECT datname FROM pg_database WHERE NOT datistemplate\"); do psql -U {user} -d \"$d\" -Atc \"SELECT count(*) FROM pg_tables WHERE schemaname NOT IN ('pg_catalog','information_schema')\"; done"
            ),
            counted,
        );
    }
    let overlay = common::scratch(test);
    let fixture = Arc::new(
        FixtureHost::open(&common::fixture_dir("cestmoi"))
            .unwrap()
            .with_case(&case)
            .unwrap()
            .with_overlay(&overlay)
            .unwrap(),
    );
    Server { host: fixture.clone(), fixture }
}

#[test]
fn the_drill_restores_every_dump_into_a_throwaway_container_and_compares_files() {
    let s = drill_server(
        "backup-drill",
        &[
            ("outline-postgres", "outline", "0\n12\n"),
            ("bookorbit-db", "bookorbit", "7\n"),
            ("supabase-db", "supabase_admin", "0\n41\n"),
            ("dokploy-postgres", "dokploy", "23\n"),
        ],
    );
    // nothing to drill yet
    assert_eq!(
        s.moi(&["backup", "--drill"]),
        (2, "there is no backup in /mnt/storage/backups yet: run `moi backup` first".to_string())
    );
    assert_eq!(s.moi(&["backup"]).0, 0);
    let before = s.fixture.docker_requests().len();
    let (code, out) = s.moi(&["backup", "--drill"]);
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], format!("drill of {RUN}"));
    assert!(
        lines.contains(&"  outline-postgres: restored into a fresh postgres:16-alpine: 12 tables")
            || lines
                .iter()
                .any(|l| l.starts_with("  outline-postgres: restored into a fresh ") && l.ends_with(": 12 tables")),
        "{out}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("  supabase-db: restored into a fresh ") && l.ends_with(": 41 tables")),
        "{out}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("  dokploy-postgres: restored into a fresh ") && l.ends_with(": 23 tables")),
        "the swarm service's dump too: {out}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("  files: archive read to its end; ")
            && l.contains(" sampled, ")
            && l.ends_with(" identical to the disk")),
        "{out}"
    );
    assert_eq!(*lines.last().unwrap(), "the backup can be restored");
    // each throwaway container: no network, the dumps read-only, and removed again
    let requests = s.fixture.docker_requests()[before..].to_vec();
    let created: Vec<&String> =
        requests.iter().filter(|r| r.starts_with("POST /containers/create?name=moi-drill-")).collect();
    assert_eq!(created.len(), 4, "{requests:?}");
    for name in ["bookorbit-db", "dokploy-postgres", "outline-postgres", "supabase-db"] {
        assert!(requests.contains(&format!("POST /containers/moi-drill-{name}/start")), "{name} started");
        assert_eq!(
            requests.iter().filter(|r| **r == format!("DELETE /containers/moi-drill-{name}?force=true&v=true")).count(),
            2,
            "{name}: a leftover is cleared before, the container removed after"
        );
    }
    // no container of the server itself was touched
    assert!(
        !requests.iter().any(|r| (r.starts_with("POST") || r.starts_with("DELETE")) && !r.contains("moi-drill-")),
        "{requests:?}"
    );
}

#[test]
fn a_dump_that_restores_to_nothing_fails_the_drill() {
    // the outline dump loads, and there is no table in what comes out
    let s = drill_server(
        "backup-drill-empty",
        &[
            ("outline-postgres", "outline", "0\n0\n"),
            ("bookorbit-db", "bookorbit", "7\n"),
            ("supabase-db", "supabase_admin", "41\n"),
            ("dokploy-postgres", "dokploy", "23\n"),
        ],
    );
    assert_eq!(s.moi(&["backup"]).0, 0);
    let (code, out) = s.moi(&["backup", "--drill"]);
    assert_eq!(code, 1);
    assert!(out.contains("  outline-postgres: FAILED: restored, and there is not a single table in it\n"), "{out}");
    assert!(out.ends_with("THE BACKUP COULD NOT BE RESTORED\n"));
    assert!(
        s.fixture
            .docker_requests()
            .contains(&"DELETE /containers/moi-drill-outline-postgres?force=true&v=true".to_string()),
        "removed also when it failed"
    );
    // a damaged archive
    let s = drill_server(
        "backup-drill-archive",
        &[
            ("outline-postgres", "outline", "3\n"),
            ("bookorbit-db", "bookorbit", "7\n"),
            ("supabase-db", "supabase_admin", "41\n"),
            ("dokploy-postgres", "dokploy", "23\n"),
        ],
    );
    assert_eq!(s.moi(&["backup"]).0, 0);
    let archive = format!("{RUN}/configs.tar.gz");
    let whole = s.host.read(Path::new(&archive)).unwrap();
    s.host.write(Path::new(&archive), &whole[..whole.len() / 2], 0o600).unwrap();
    let (code, out) = s.moi(&["backup", "--drill"]);
    assert_eq!(code, 1);
    assert!(out.contains("  files: FAILED: the archive is damaged: "), "{out}");
}
