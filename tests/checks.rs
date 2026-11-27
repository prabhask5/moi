//! Check engine, custom checks, pushing and `moi check` (docs/spec/05-checks.md § Engine).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cestmoi::checks::{self, Check, Cx, Memory, Mode, Outcome, Status};
use cestmoi::cli;
use cestmoi::config::{self, Config, Paths};
use cestmoi::host::{FixtureHost, Host};
use cestmoi::state::StateDir;
use cestmoi::state::db::Db;

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
    overlay: PathBuf,
}

fn server(test: &str) -> Server {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    Server { host: fixture.clone(), fixture, overlay }
}

impl Server {
    fn config(&self) -> Arc<Config> {
        Arc::new(config::load(self.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap().config)
    }

    fn edit_config(&self, edit: impl FnOnce(String) -> String) {
        let path = Path::new("/etc/moi/moi.toml");
        let text = edit(self.host.read_to_string(path).unwrap());
        self.host.write(path, text.as_bytes(), 0o644).unwrap();
    }

    fn db(&self) -> Db {
        let state = StateDir::new("/var/lib/moi");
        state.ensure(self.host.as_ref()).unwrap();
        state.open_db(self.host.as_ref()).unwrap()
    }

    fn check(&self, name: &str) -> Arc<dyn Check> {
        checks::enabled(&self.config())
            .into_iter()
            .find(|c| c.name() == name)
            .unwrap_or_else(|| panic!("{name} not enabled"))
    }

    /// (exit code, stdout) of an in-process `moi …`.
    fn moi(&self, args: &[&str]) -> (u8, String) {
        let mut out = Vec::new();
        let code = match cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out) {
            Ok(()) => 0,
            Err(e) => {
                if e.exit_code() != 1 {
                    out.extend_from_slice(e.to_string().as_bytes());
                }
                e.exit_code()
            }
        };
        (code, String::from_utf8(out).unwrap())
    }
}

// ------------------------------------------------------------------ memory: deltas and sustained conditions

#[test]
fn a_condition_is_reported_only_after_holding_for_the_configured_number_of_runs() {
    let mut m = Memory::default();
    assert!(!m.sustain("cpu", true, 3));
    assert!(!m.sustain("cpu", true, 3));
    assert!(m.sustain("cpu", true, 3), "third run in a row");
    assert!(m.sustain("cpu", true, 3), "and it keeps being reported while it holds");
    assert!(!m.sustain("cpu", false, 3));
    assert!(!m.sustain("cpu", true, 3), "a pause starts the count again");
    assert!(m.sustain("swap", true, 1), "one run is enough when asked for");
}

#[test]
fn a_recorded_run_stores_its_memory_and_result_and_a_dry_run_stores_nothing() {
    let s = server("checks-modes");
    let (config, db, resources) = (s.config(), s.db(), s.check("resources"));
    // dry: the first-ever run has no CPU delta, and leaves nothing behind
    let dry = checks::run(&s.host, &config, &db, &resources, Mode::Dry).unwrap();
    assert_eq!((dry.status, dry.message.as_str()), (Status::Up, "OK: RAM 38%, swap 2%, CPU 48C"));
    assert_eq!(Memory::load(&db, "resources").unwrap(), Memory::default());
    assert_eq!(db.conn().query_row("SELECT count(*) FROM check_results", [], |r| r.get::<_, i64>(0)).unwrap(), 0);

    // recorded: counters and restart counts are kept for the next run
    checks::run(&s.host, &config, &db, &resources, Mode::Record).unwrap();
    let memory = Memory::load(&db, "resources").unwrap();
    assert_eq!(memory.values["cpu"], "9362000 8050000");
    assert!(memory.values["restarts"].contains("\"jellyfin\":0"));
    let (status, message): (String, String) = db
        .conn()
        .query_row("SELECT status, message FROM check_results WHERE check_name = 'resources'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((status.as_str(), message.as_str()), ("up", "OK: RAM 38%, swap 2%, CPU 48C"));

    // the machine works for five minutes at 95 % CPU: a dry run sees it, any number of times, without counting it
    s.host.write(Path::new("/proc/stat"), b"cpu  1028500 2000 300000 8001000 50500 0 10000 0 0 0\n", 0o644).unwrap();
    for _ in 0..3 {
        let dry = checks::run(&s.host, &config, &db, &resources, Mode::Dry).unwrap();
        assert_eq!(dry.message, "OK: CPU 95%, RAM 38%, swap 2%, CPU 48C");
    }
    assert_eq!(Memory::load(&db, "resources").unwrap().sustained.get("cpu"), None);
    // recorded runs count: hot once is not an alert, hot twice is
    let first = checks::run(&s.host, &config, &db, &resources, Mode::Record).unwrap();
    assert_eq!(first.status, Status::Up);
    s.host.write(Path::new("/proc/stat"), b"cpu  1057000 2000 300000 8002000 51000 0 10000 0 0 0\n", 0o644).unwrap();
    let second = checks::run(&s.host, &config, &db, &resources, Mode::Record).unwrap();
    assert_eq!(
        (second.status, second.message.as_str()),
        (Status::Down, "Problem: CPU 95% for 10+ min | CPU 95%, RAM 38%, swap 2%, CPU 48C")
    );
    // it cools down: the condition is forgotten
    s.host.write(Path::new("/proc/stat"), b"cpu  1058000 2000 300000 8030000 52000 0 10000 0 0 0\n", 0o644).unwrap();
    let third = checks::run(&s.host, &config, &db, &resources, Mode::Record).unwrap();
    assert_eq!(third.status, Status::Up);
    assert_eq!(Memory::load(&db, "resources").unwrap().sustained.get("cpu"), None);
    assert_eq!(db.conn().query_row("SELECT count(*) FROM check_results", [], |r| r.get::<_, i64>(0)).unwrap(), 4);
}

#[test]
fn the_sustain_text_follows_the_configured_runs_and_interval() {
    let s = server("checks-sustain-text");
    s.edit_config(|t| t.replace("[checks.resources]\n", "[checks.resources]\ninterval = \"2m\"\nsustain_runs = 3\n"));
    let (config, db, resources) = (s.config(), s.db(), s.check("resources"));
    let mut memory = Memory::default();
    memory.values.insert("cpu".into(), "9332000 8048500".into());
    memory.sustained.insert("cpu".into(), 2);
    memory.save(&db, "resources", 0).unwrap();
    let result = checks::run(&s.host, &config, &db, &resources, Mode::Dry).unwrap();
    assert_eq!(result.message, "Problem: CPU 95% for 6+ min | CPU 95%, RAM 38%, swap 2%, CPU 48C");
}

// ------------------------------------------------------------------ a check that dies still reports

struct Fake {
    behaviour: fn() -> Outcome,
    timeout: Duration,
}

impl Check for Fake {
    fn name(&self) -> String {
        "fake".into()
    }
    fn timeout(&self, _: &Config) -> Duration {
        self.timeout
    }
    fn monitor(&self, _: &Config) -> Option<String> {
        None
    }
    fn journal_tag(&self, _: &Config) -> String {
        "moi-check-fake".into()
    }
    fn run(&self, _: &Cx, memory: &mut Memory) -> Outcome {
        memory.values.insert("touched".into(), "yes".into());
        (self.behaviour)()
    }
}

#[test]
fn a_check_that_panics_or_hangs_is_reported_as_down_and_remembers_nothing() {
    let s = server("checks-crash");
    let (config, db) = (s.config(), s.db());
    let crashing: Arc<dyn Check> =
        Arc::new(Fake { behaviour: || panic!("division by zero"), timeout: Duration::from_secs(5) });
    let result = checks::run(&s.host, &config, &db, &crashing, Mode::Record).unwrap();
    assert_eq!((result.status, result.message.as_str()), (Status::Down, "check crashed: division by zero"));

    let hanging: Arc<dyn Check> = Arc::new(Fake {
        behaviour: || {
            std::thread::sleep(Duration::from_secs(3));
            Outcome::up("late")
        },
        timeout: Duration::from_millis(100),
    });
    let started = std::time::Instant::now();
    let result = checks::run(&s.host, &config, &db, &hanging, Mode::Record).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "a hung check must not hang the engine");
    assert_eq!((result.status, result.message.as_str()), (Status::Down, "check crashed: timed out after 0s"));
    assert_eq!(
        Memory::load(&db, "fake").unwrap(),
        Memory::default(),
        "a run that did not finish leaves the memory as it was"
    );
    assert_eq!(
        db.conn()
            .query_row("SELECT count(*) FROM check_results WHERE status = 'down'", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

// ------------------------------------------------------------------ storage details

#[test]
fn the_write_test_cleans_up_after_itself() {
    let s = server("checks-storage-probe");
    let (config, db, storage) = (s.config(), s.db(), s.check("storage"));
    let result = checks::run(&s.host, &config, &db, &storage, Mode::Dry).unwrap();
    assert_eq!(result.message, "OK: nvme0n1 3% worn, nvme0n1 34C, sda 31C, / 31% used, /mnt/storage 19% used");
    assert!(!s.host.exists(Path::new("/mnt/storage/.storage-check")));
    assert!(!s.overlay.join("mnt/storage/.storage-check").exists());
}

#[test]
fn disks_are_discovered_when_not_listed() {
    let s = server("checks-storage-auto");
    s.edit_config(|t| {
        let start = t.find("disks = [").unwrap();
        let end = t[start..].find("]\n").unwrap() + start + 2;
        format!("{}{}", &t[..start], &t[end..])
    });
    // discovery finds nvme0n1 (type nvme) and sda (type auto); loop devices are not disks
    s.fixture.add_command(
        "smartctl --json -H -A -d auto /dev/sda",
        0,
        r#"{"ata_smart_attributes":{"table":[{"id":194,"raw":{"string":"33"}}]}}"#,
    );
    let result = checks::run(&s.host, &s.config(), &s.db(), &s.check("storage"), Mode::Dry).unwrap();
    assert_eq!(result.message, "OK: nvme0n1 3% worn, nvme0n1 34C, sda 33C, / 31% used, /mnt/storage 19% used");
    assert!(!s.fixture.commands_run().iter().any(|c| c.contains("loop0")));
}

// ------------------------------------------------------------------ custom checks

#[test]
fn a_custom_check_is_up_on_exit_0_and_its_first_line_is_the_message() {
    let s = server("checks-custom");
    s.edit_config(|t| {
        format!("{t}\n[[checks.custom]]\nname = \"certs\"\nrun = \"/usr/local/bin/check-certs --days 14\"\ninterval = \"1h\"\nmonitor = \"Certificates\"\n\n[[checks.custom]]\nname = \"quiet\"\nrun = \"/usr/local/bin/quiet\"\n\n[[checks.custom]]\nname = \"gone\"\nrun = \"/usr/local/bin/gone\"\n")
    });
    let (config, db) = (s.config(), s.db());
    let names: Vec<String> = checks::enabled(&config).iter().map(|c| c.name()).collect();
    assert_eq!(names, ["resources", "storage", "certs", "quiet", "gone"]);

    s.fixture.add_command(
        "/usr/local/bin/check-certs --days 14",
        0,
        "all certificates valid for 60+ days\nsecond line ignored\n",
    );
    let up = checks::run(&s.host, &config, &db, &s.check("certs"), Mode::Dry).unwrap();
    assert_eq!(
        (up.status, up.message.as_str(), up.monitor.as_deref()),
        (Status::Up, "all certificates valid for 60+ days", Some("Certificates"))
    );

    s.fixture.add_command("/usr/local/bin/check-certs --days 14", 2, "notes.example.com expires in 3 days\n");
    let down = checks::run(&s.host, &config, &db, &s.check("certs"), Mode::Dry).unwrap();
    assert_eq!((down.status, down.message.as_str()), (Status::Down, "notes.example.com expires in 3 days"));

    // silent scripts get a message anyway
    s.fixture.add_command("/usr/local/bin/quiet", 0, "");
    assert_eq!(checks::run(&s.host, &config, &db, &s.check("quiet"), Mode::Dry).unwrap().message, "OK");
    s.fixture.add_command("/usr/local/bin/quiet", 3, "");
    assert_eq!(checks::run(&s.host, &config, &db, &s.check("quiet"), Mode::Dry).unwrap().message, "exit status 3");
    // a script that cannot be started is a failing check, not a crash
    let gone = checks::run(&s.host, &config, &db, &s.check("gone"), Mode::Dry).unwrap();
    assert_eq!(gone.status, Status::Down);
    assert!(gone.message.starts_with("cannot run /usr/local/bin/gone: "), "{}", gone.message);
}

// ------------------------------------------------------------------ pushing

fn bind(db: &Db, check: &str, monitor: &str, token: &str) {
    db.conn()
        .execute(
            "INSERT INTO monitor_bindings (owner, monitor_name, push_token, updated_at) VALUES (?1, ?2, ?3, 0)",
            (format!("check:{check}"), monitor, token),
        )
        .unwrap();
}

/// A fake Kuma that records every request line and answers 200.
fn kuma(s: &Server) -> Arc<Mutex<Vec<String>>> {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    s.fixture.set_http(Arc::new(move |authority, raw| {
        let line = String::from_utf8_lossy(raw).lines().next().unwrap_or_default().to_string();
        log.lock().unwrap().push(format!("{authority} {line}"));
        b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}".to_vec()
    }));
    seen
}

#[test]
fn moi_check_without_push_never_contacts_kuma() {
    let s = server("checks-no-push");
    let requests = kuma(&s);
    bind(&s.db(), "resources", "Server Resources", "FAKEresourcesTok01");
    let (code, out) = s.moi(&["check"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out,
        "[resources] up: OK: RAM 38%, swap 2%, CPU 48C\n[storage] up: OK: nvme0n1 3% worn, nvme0n1 34C, sda 31C, / 31% used, /mnt/storage 19% used\n"
    );
    assert_eq!(*requests.lock().unwrap(), Vec::<String>::new());
    assert_eq!(s.fixture.journal_entries().len(), 0);
}

#[test]
fn push_sends_one_status_and_one_message_to_the_bound_monitor() {
    let s = server("checks-push");
    let requests = kuma(&s);
    bind(&s.db(), "storage", "Server Storage", "FAKEstorageTok0003");
    s.fixture.set_env("JOURNAL_STREAM", "8:1");
    let (code, out) = s.moi(&["check", "storage", "--push"]);
    assert_eq!(
        (code, out.as_str()),
        (0, "up: OK: nvme0n1 3% worn, nvme0n1 34C, sda 31C, / 31% used, /mnt/storage 19% used\n")
    );
    let sent = requests.lock().unwrap().clone();
    assert_eq!(
        sent,
        [
            "uptime.home:80 GET /api/push/FAKEstorageTok0003?status=up&msg=OK%3A%20nvme0n1%203%25%20worn%2C%20nvme0n1%2034C%2C%20sda%2031C%2C%20%2F%2031%25%20used%2C%20%2Fmnt%2Fstorage%2019%25%20used&ping= HTTP/1.1"
        ]
    );
    assert_eq!(sent[0].matches("status=").count(), 1, "exactly one status (the legacy backup push sent two)");
    assert_eq!(sent[0].matches("msg=").count(), 1);
    // the result is in the journal under the check's legacy tag
    let entry = &s.fixture.journal_entries()[0];
    assert_eq!(entry[0].1, "up: OK: nvme0n1 3% worn, nvme0n1 34C, sda 31C, / 31% used, /mnt/storage 19% used");
    assert_eq!(entry[2], ("SYSLOG_IDENTIFIER".to_string(), "storage-check".to_string()));
}

#[test]
fn a_push_that_cannot_happen_says_why_and_never_fails_the_check() {
    let s = server("checks-push-problems");
    let requests = kuma(&s);
    // not bound yet
    let (code, out) = s.moi(&["check", "resources", "--push"]);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "up: OK: RAM 38%, swap 2%, CPU 48C\nnot pushed: monitor \"Server Resources\" is not bound to this check yet (run `moi monitors sync`)\n"
    );
    // bound, but Kuma answers with an error: three attempts (curl --retry 2), then the reason
    bind(&s.db(), "resources", "Server Resources", "FAKEresourcesTok01");
    s.fixture.set_http(Arc::new({
        let log = requests.clone();
        move |_, _| {
            log.lock().unwrap().push("attempt".into());
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
        }
    }));
    let (code, out) = s.moi(&["check", "resources", "--push", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["checks"][0]["pushed"], "push failed: HTTP 404");
    assert_eq!(requests.lock().unwrap().len(), 3);
    // shadow mode (migration): checks run, nothing is pushed
    s.edit_config(|t| t.replace("[monitors.kuma]", "[monitors]\nmode = \"shadow\"\n\n[monitors.kuma]"));
    let before = requests.lock().unwrap().len();
    let (_, out) = s.moi(&["check", "resources", "--push"]);
    assert!(out.ends_with("not pushed: monitors.mode is shadow\n"), "{out}");
    assert_eq!(requests.lock().unwrap().len(), before);
}

// ------------------------------------------------------------------ the command

#[test]
fn moi_check_exits_1_when_something_is_down_and_prints_json_on_request() {
    let s = server("checks-cli");
    s.host
        .write(
            Path::new("/proc/meminfo"),
            b"MemTotal: 16163412 kB\nMemAvailable: 9876540 kB\nSwapTotal: 4194300 kB\nSwapFree: 400000 kB\n",
            0o644,
        )
        .unwrap();
    let (code, out) = s.moi(&["check"]);
    assert_eq!(code, 1);
    assert!(
        out.starts_with(
            "[resources] down: Problem: swap 90% used (memory pressure) | RAM 38%, swap 90%, CPU 48C\n[storage] up: "
        ),
        "{out}"
    );
    // one check alone prints exactly what the legacy script printed
    let (code, out) = s.moi(&["check", "resources"]);
    assert_eq!(
        (code, out.as_str()),
        (1, "down: Problem: swap 90% used (memory pressure) | RAM 38%, swap 90%, CPU 48C\n")
    );

    let (code, out) = s.moi(&["check", "--json"]);
    assert_eq!(code, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((v["ok"].as_bool(), v["down"].as_u64()), (Some(true), Some(1)));
    assert_eq!(v["checks"][0]["name"], "resources");
    assert_eq!(v["checks"][0]["status"], "down");
    assert_eq!(v["checks"][0]["monitor"], "Server Resources");
    assert!(v["checks"][0]["pushed"].is_null());
    assert_eq!(v["checks"][1]["status"], "up");

    let (code, out) = s.moi(&["check", "nope"]);
    assert_eq!((code, out.as_str()), (2, "no such check: nope (enabled: resources, storage)"));
}

#[test]
fn a_server_without_checks_says_how_to_add_one() {
    let overlay = common::scratch("checks-none");
    let host: Arc<dyn Host> =
        Arc::new(FixtureHost::open(&common::fixture_dir("minimal")).unwrap().with_overlay(&overlay).unwrap());
    let mut out = Vec::new();
    cli::execute(&cli::parse(&["check"]).unwrap(), host, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "no checks are enabled: add [checks.resources], [checks.storage] or [[checks.custom]] to moi.toml\n"
    );
}
