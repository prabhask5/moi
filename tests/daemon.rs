//! The daemon: scheduling, the check and daily jobs, `moi status` (docs/spec/10-daemon.md).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cestmoi::checks::Memory;
use cestmoi::cli;
use cestmoi::config::{self, Paths};
use cestmoi::daemon::{self, Daemon, Outcome};
use cestmoi::host::{FixtureHost, Host, parse_rfc3339, unix_seconds};
use cestmoi::state::StateDir;
use cestmoi::state::db::Db;

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
}

/// The cestmoi fixture: Thursday 26 November 2026, 17:00 UTC — noon in New York.
fn server(test: &str) -> Server {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    Server { host: fixture.clone(), fixture }
}

impl Server {
    fn daemon(&self) -> Daemon {
        let loaded = config::load(self.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
        Daemon::new(self.host.clone(), loaded).unwrap()
    }

    fn db(&self) -> Db {
        StateDir::new("/var/lib/moi").open_db(self.host.as_ref()).unwrap()
    }

    fn edit_config(&self, edit: impl FnOnce(String) -> String) {
        let path = Path::new("/etc/moi/moi.toml");
        let text = edit(self.host.read_to_string(path).unwrap());
        self.host.write(path, text.as_bytes(), 0o644).unwrap();
    }

    fn moi(&self, args: &[&str]) -> (u8, String) {
        let mut out = Vec::new();
        match cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out) {
            Ok(()) => (0, String::from_utf8(out).unwrap()),
            Err(e) if e.exit_code() == 1 => (1, String::from_utf8(out).unwrap()),
            Err(e) => (e.exit_code(), e.to_string()),
        }
    }

    /// Pushes Kuma received, as `token status msg` (everything else on the network answers 200).
    fn kuma(&self) -> Arc<Mutex<Vec<String>>> {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        self.fixture.set_http(Arc::new(move |_, raw| {
            let line = String::from_utf8_lossy(raw).lines().next().unwrap_or_default().to_string();
            if let Some(rest) = line.strip_prefix("GET /api/push/") {
                let (token, query) = rest.split_once('?').unwrap();
                let param = |name: &str| {
                    query
                        .split(['&', ' '])
                        .find_map(|p| p.strip_prefix(&format!("{name}=")))
                        .unwrap_or_default()
                        .replace("%20", " ")
                        .replace("%3A", ":")
                        .replace("%2C", ",")
                        .replace("%25", "%")
                        .replace("%2F", "/")
                };
                log.lock().unwrap().push(format!("{token} {} {}", param("status"), param("msg")));
            }
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}".to_vec()
        }));
        seen
    }

    fn bind(&self, owner: &str, token: &str) {
        self.db()
            .conn()
            .execute(
                "INSERT INTO monitor_bindings (owner, monitor_name, push_token, updated_at) VALUES (?1, ?1, ?2, 0)",
                (owner, token),
            )
            .unwrap();
    }
}

fn at(time: &str) -> i64 {
    unix_seconds(parse_rfc3339(time).unwrap())
}

/// (name, next) of every job.
fn plan(daemon: &Daemon) -> Vec<(String, i64)> {
    daemon.jobs().iter().map(|j| (j.name.clone(), j.next)).collect()
}

fn names(ran: &[(String, Outcome)]) -> Vec<&str> {
    ran.iter().map(|(name, _)| name.as_str()).collect()
}

const START: &str = "2026-11-26T17:00:00Z";

#[test]
fn checks_run_at_start_and_then_on_their_interval_without_drift() {
    let s = server("daemon-intervals");
    s.fixture.set_env("JOURNAL_STREAM", "8:1");
    let mut daemon = s.daemon();
    let start = at(START);
    assert_eq!(
        plan(&daemon),
        [
            ("check:resources".to_string(), start),
            ("check:storage".to_string(), start),
            ("check:services".to_string(), start),
            ("daily".to_string(), at("2026-11-27T10:00:00Z"))
        ],
        "checks are due at once; the daily job at 05:00 local time"
    );
    let ran = daemon.tick().unwrap();
    assert_eq!(names(&ran), ["check:resources", "check:storage", "check:services"]);
    assert_eq!((ran[0].1.status.as_str(), ran[0].1.message.as_str()), ("up", "OK: RAM 38%, swap 2%, CPU 48C"));
    assert_eq!(ran[2].1.message, "All 35 containers + 7 swarm services running, 17 app, 4 database, 10 edge checks OK");
    // recorded: results, and what the next run needs
    let db = s.db();
    assert_eq!(db.conn().query_row("SELECT count(*) FROM check_results", [], |r| r.get::<_, i64>(0)).unwrap(), 3);
    assert_eq!(Memory::load(&db, "resources").unwrap().values["cpu"], "9362000 8050000");
    // in the journal under each check's own tag
    let tags: Vec<String> = s
        .fixture
        .journal_entries()
        .iter()
        .map(|e| e.iter().find(|(k, _)| k == "SYSLOG_IDENTIFIER").unwrap().1.clone())
        .collect();
    assert_eq!(tags, ["resource-check", "storage-check", "service-check"]);
    assert_eq!(s.fixture.journal_entries()[0][0].1, "up: OK: RAM 38%, swap 2%, CPU 48C");

    // nothing is due until five minutes have passed
    assert_eq!(daemon.tick().unwrap(), []);
    s.fixture.advance(Duration::from_secs(299));
    assert_eq!(daemon.tick().unwrap(), []);
    s.fixture.advance(Duration::from_secs(1));
    assert_eq!(daemon.tick().unwrap().len(), 3);
    // a run that starts late does not push the schedule back: still on the five-minute grid
    s.fixture.advance(Duration::from_secs(317));
    assert_eq!(daemon.tick().unwrap().len(), 3);
    assert_eq!(daemon.jobs()[0].next, start + 900);
    // ticks that passed while nothing could run are skipped, not made up for
    s.fixture.advance(Duration::from_secs(3 * 3600));
    assert_eq!(daemon.tick().unwrap().len(), 3);
    assert_eq!(daemon.tick().unwrap(), []);
    let next = daemon.jobs()[0].next;
    assert_eq!(((next - start) % 300, next > start + 3 * 3600 + 617), (0, true));
    assert_eq!(db.conn().query_row("SELECT count(*) FROM check_results", [], |r| r.get::<_, i64>(0)).unwrap(), 12);
}

#[test]
fn every_scheduled_check_is_pushed_once_and_a_manual_check_changes_nothing() {
    let s = server("daemon-push");
    let pushes = s.kuma();
    let mut daemon = s.daemon();
    s.bind("check:resources", "FAKEresourcesTok01");
    s.bind("check:storage", "FAKEstorageTok0003");
    daemon.tick().unwrap();
    assert_eq!(
        *pushes.lock().unwrap(),
        [
            "FAKEresourcesTok01 up OK: RAM 38%, swap 2%, CPU 48C",
            "FAKEstorageTok0003 up OK: nvme0n1 3% worn, nvme0n1 34C, sda 31C, / 31% used, /mnt/storage 19% used"
        ],
        "the services check has no bound monitor yet: nothing is pushed for it"
    );
    // the machine works hard for five minutes
    s.host.write(Path::new("/proc/stat"), b"cpu  1028500 2000 300000 8001000 50500 0 10000 0 0 0\n", 0o644).unwrap();
    // someone runs the check by hand, several times: what the daemon stored stays exactly as it was
    let stored = Memory::load(&s.db(), "resources").unwrap();
    for _ in 0..3 {
        assert_eq!(s.moi(&["check", "resources"]).1, "up: OK: CPU 95%, RAM 38%, swap 2%, CPU 48C\n");
    }
    assert_eq!(Memory::load(&s.db(), "resources").unwrap(), stored);
    assert_eq!(pushes.lock().unwrap().len(), 2, "and nothing was pushed");
    // the daemon's next run counts it once: hot for one run is not an alert yet
    s.fixture.advance(Duration::from_secs(300));
    let ran = daemon.tick().unwrap();
    assert_eq!(ran[0].1.message, "OK: CPU 95%, RAM 38%, swap 2%, CPU 48C");
    assert_eq!(Memory::load(&s.db(), "resources").unwrap().sustained["cpu"], 1);
    assert_eq!(pushes.lock().unwrap().len(), 4);

    // shadow mode (migration): checks run and are recorded, nothing is pushed
    s.edit_config(|t| t.replace("[monitors.kuma]", "[monitors]\nmode = \"shadow\"\n\n[monitors.kuma]"));
    let mut shadow = s.daemon();
    assert_eq!(shadow.tick().unwrap().len(), 3);
    assert_eq!(pushes.lock().unwrap().len(), 4);
}

#[test]
fn the_daily_job_snapshots_audits_and_reports_freshness() {
    let s = server("daemon-daily");
    let pushes = s.kuma();
    s.fixture.set_env("JOURNAL_STREAM", "8:1");
    let mut daemon = s.daemon();
    s.bind("freshness:snapshot", "FAKEfreshnessTok05");
    daemon.tick().unwrap();
    assert!(!s.host.exists(Path::new("/var/lib/moi/state/CONTEXT.md")), "not before its time");
    s.fixture.set_now(parse_rfc3339("2026-11-27T10:00:00Z").unwrap());
    let ran = daemon.tick().unwrap();
    let daily = ran.iter().find(|(name, _)| name == "daily").expect("the daily job ran at 05:00 local time");
    assert_eq!((daily.1.status.as_str(), daily.1.message.as_str()), ("ok", "snapshot OK, 1 changes today"));
    for file in ["state/snapshot/meta/hostname", "state/CONTEXT.md", "state/audit.md"] {
        assert!(s.host.exists(&Path::new("/var/lib/moi").join(file)), "{file}");
    }
    assert_eq!(pushes.lock().unwrap().last().unwrap(), "FAKEfreshnessTok05 up snapshot OK, 1 changes today");
    // what a sync would change is said once a day, and never done from here
    let said: Vec<String> = s.fixture.journal_entries().iter().map(|e| e[0].1.clone()).collect();
    assert!(
        said.iter().any(|m| m == "monitors: 40 changes and 0 differences pending (moi monitors sync --dry-run)"),
        "{said:?}"
    );
    assert!(
        s.fixture.docker_requests().iter().all(|r| r.starts_with("GET ") || r.contains("/exec")),
        "the daemon changed nothing in Docker"
    );
    // tomorrow, same time; nothing changed on the machine, so nothing new is recorded
    assert_eq!(daemon.jobs().iter().find(|j| j.name == "daily").unwrap().next, at("2026-11-28T10:00:00Z"));
    s.fixture.set_now(parse_rfc3339("2026-11-28T10:00:00Z").unwrap());
    let ran = daemon.tick().unwrap();
    assert_eq!(ran.iter().find(|(name, _)| name == "daily").unwrap().1.message, "snapshot OK, 0 changes today");
}

#[test]
fn a_calendar_job_missed_while_the_machine_was_off_runs_five_minutes_after_start() {
    let daily = |daemon: &Daemon| daemon.jobs().iter().find(|j| j.name == "daily").unwrap().next;
    let start = at(START);
    let last_run = |s: &Server, when: &str| {
        s.daemon();
        s.db()
            .conn()
            .execute(
                "INSERT OR REPLACE INTO schedule_runs (job, last_run, last_status) VALUES ('daily', ?1, 'ok')",
                [at(when)],
            )
            .unwrap();
    };
    // never run (a new install): wait for its time
    let s = server("daemon-persistent-new");
    assert_eq!(daily(&s.daemon()), at("2026-11-27T10:00:00Z"));
    // ran this morning: next is tomorrow
    let s = server("daemon-persistent-today");
    last_run(&s, "2026-11-26T10:00:04Z");
    assert_eq!(daily(&s.daemon()), at("2026-11-27T10:00:00Z"));
    // last ran two days ago — the machine was off this morning: catch up shortly after start, not at once
    let s = server("daemon-persistent-missed");
    last_run(&s, "2026-11-24T10:00:04Z");
    let mut daemon = s.daemon();
    assert_eq!(daily(&daemon), start + 300);
    assert!(!names(&daemon.tick().unwrap()).contains(&"daily"));
    s.fixture.advance(Duration::from_secs(300));
    assert!(names(&daemon.tick().unwrap()).contains(&"daily"));
    assert_eq!(daily(&daemon), at("2026-11-27T10:00:00Z"), "then back on its calendar");
    // and a restart right after does not run it again
    assert_eq!(daily(&s.daemon()), at("2026-11-27T10:00:00Z"));
}

#[test]
fn a_job_that_fails_is_reported_and_tried_again_at_its_next_time() {
    // a machine on which part of the state directory cannot be written (a full or failing disk)
    let case = common::scratch("daemon-failure-case");
    std::fs::write(case.join("unwritable"), "/var/lib/moi/state/snapshot\n").unwrap();
    let overlay = common::scratch("daemon-failure");
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
    let mut daemon = s.daemon();
    s.bind("freshness:snapshot", "FAKEfreshnessTok05");
    daemon.trigger("daily");
    let ran = daemon.tick().unwrap();
    let daily = &ran.iter().find(|(name, _)| name == "daily").unwrap().1;
    assert_eq!(daily.status, "failed", "{}", daily.message);
    assert!(
        pushes.lock().unwrap().last().unwrap().starts_with("FAKEfreshnessTok05 down snapshot failed: "),
        "{:?}",
        pushes.lock().unwrap()
    );
    // the daemon lives on: the other jobs ran, and the daily job waits for its next time
    assert_eq!(names(&ran)[..3], ["check:resources", "check:storage", "check:services"]);
    assert_eq!(daemon.jobs().iter().find(|j| j.name == "daily").unwrap().next, at("2026-11-27T10:00:00Z"));
    assert_eq!(
        s.db()
            .conn()
            .query_row("SELECT last_status FROM schedule_runs WHERE job = 'daily'", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "failed"
    );
}

#[test]
fn status_says_what_ran_and_what_comes_next() {
    let s = server("daemon-status");
    let mut daemon = s.daemon();
    daemon.tick().unwrap();
    let status = daemon.status();
    assert_eq!(
        (status["running"].as_bool(), status["started"].as_str(), status["jobs"].as_array().unwrap().len()),
        (Some(true), Some(START), 4)
    );
    assert_eq!(
        status["jobs"][0],
        serde_json::json!({
            "name": "check:resources", "running": false, "next": "2026-11-26T17:05:00Z",
            "last": { "at": START, "status": "up", "message": "OK: RAM 38%, swap 2%, CPU 48C", "duration_ms": status["jobs"][0]["last"]["duration_ms"] },
        })
    );
    assert_eq!(
        status["jobs"][3],
        serde_json::json!({ "name": "daily", "running": false, "next": "2026-11-27T10:00:00Z", "last": null })
    );
    // asking for a job by name makes it due now; an unknown name is refused
    assert!(daemon.trigger("daily") && !daemon.trigger("nope"));
    assert!(names(&daemon.tick().unwrap()).contains(&"daily"));
}

#[test]
fn moi_status_asks_the_running_daemon_and_says_when_there_is_none() {
    let s = server("daemon-ipc");
    // no daemon: exit 1, and what the last one left behind
    let (code, out) = s.moi(&["status"]);
    assert_eq!((code, out.as_str()), (1, "daemon: not running (systemctl status moi)\n"));
    s.daemon().tick().unwrap();
    let (code, out) = s.moi(&["status"]);
    assert_eq!(code, 1);
    assert!(out.contains("  check:resources    last up at 2026-11-26 17:00 UTC\n"), "{out}");

    // the service, as systemd would start it
    s.fixture.set_env("NOTIFY_SOCKET", "/run/systemd/notify");
    s.fixture.set_env("WATCHDOG_USEC", "60000000");
    let daemon = s.daemon();
    std::thread::spawn(move || daemon.run());
    let asked = (0..200).find_map(|_| {
        std::thread::sleep(Duration::from_millis(25));
        daemon::ask(s.host.as_ref(), &serde_json::json!({ "cmd": "status" }))
            .filter(|status| status["jobs"][2]["last"].is_object())
    });
    let status = asked.expect("the daemon answers on its socket and has run its checks");
    assert_eq!(status["jobs"][0]["last"]["message"], "OK: RAM 38%, swap 2%, CPU 48C");
    let told = s.fixture.notifications();
    assert_eq!(told[0], "READY=1");
    assert!(told[1].starts_with("WATCHDOG=1\nSTATUS=4 jobs, "), "{told:?}");

    let (code, out) = s.moi(&["status"]);
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], format!("daemon: running since 2026-11-26 17:00 UTC (moi {})", env!("CARGO_PKG_VERSION")));
    assert_eq!(lines[1], "  check:resources    up: OK: RAM 38%, swap 2%, CPU 48C");
    assert_eq!(lines[2], "                     next 2026-11-26 17:05 UTC");
    assert_eq!(lines[7], "  daily              not run yet");
    let (_, json) = s.moi(&["status", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        (v["ok"].as_bool(), v["running"].as_bool(), v["jobs"][3]["name"].as_str()),
        (Some(true), Some(true), Some("daily"))
    );
    // the socket is private to root, and requests it does not know are refused
    use std::os::unix::fs::PermissionsExt;
    let socket = common::scratch_path("daemon-ipc").join("run/moi.sock");
    assert_eq!(std::fs::metadata(socket).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        daemon::ask(s.host.as_ref(), &serde_json::json!({ "cmd": "run", "job": "nope" })).unwrap(),
        serde_json::json!({ "ok": false, "error": "no such job: nope" })
    );
    assert_eq!(
        daemon::ask(s.host.as_ref(), &serde_json::json!({ "cmd": "run", "job": "daily" })).unwrap(),
        serde_json::json!({ "ok": true })
    );
    let ran = (0..400).any(|_| {
        std::thread::sleep(Duration::from_millis(25));
        daemon::ask(s.host.as_ref(), &serde_json::json!({ "cmd": "status" }))
            .is_some_and(|status| status["jobs"][3]["last"]["status"] == "ok")
    });
    assert!(ran, "a triggered job runs without waiting for its time");
}
