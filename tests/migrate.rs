//! `moi migrate`: from legacy scripts, cron jobs and units to moi (docs/spec/11-migration.md).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use cestmoi::cli;
use cestmoi::config::{self, Paths};
use cestmoi::daemon::Daemon;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::migrate::compare::{self, Verdict};
use cestmoi::migrate::shadow;
use cestmoi::state::StateDir;
use cestmoi::state::db::Db;

const KUMA_DB: &str = "/opt/services/monitoring/uptime-kuma/data/kuma.db";

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
}

/// The cestmoi server with what its legacy scripts left behind (`tests/fixtures/cases/legacy-state`).
fn server(test: &str) -> Server {
    let case = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cases/legacy-state");
    let fixture = FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_case(&case).unwrap();
    let fixture = Arc::new(fixture.with_overlay(&common::scratch(test)).unwrap());
    let s = Server { host: fixture.clone(), fixture };
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    s
}

impl Server {
    fn moi(&self, args: &[&str]) -> (u8, String) {
        let mut out = Vec::new();
        match cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out) {
            Ok(()) => (0, String::from_utf8(out).unwrap()),
            Err(e) if e.exit_code() == 1 => (1, String::from_utf8(out).unwrap()),
            Err(e) => (e.exit_code(), e.to_string()),
        }
    }

    fn db(&self) -> Db {
        StateDir::new("/var/lib/moi").open_db(self.host.as_ref()).unwrap()
    }

    fn setting(&self, sql: &str) -> String {
        self.db().conn().query_row(sql, [], |r| r.get::<_, String>(0)).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    /// A copy of Kuma's database that the test may add legacy results to.
    fn kuma(&self) -> rusqlite::Connection {
        let content = self.host.read(Path::new(KUMA_DB)).unwrap();
        self.host.write(Path::new(KUMA_DB), &content, 0o644).unwrap();
        rusqlite::Connection::open(self.host.real_path(Path::new(KUMA_DB)).unwrap()).unwrap()
    }
}

#[test]
fn plan_names_what_replaces_what_on_cestmoi() {
    let s = server("migrate-plan");
    let before = s.fixture.commands_run().len();
    let (code, out) = s.moi(&["migrate", "plan"]);
    assert_eq!(code, 0, "{out}");
    common::assert_golden(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/cestmoi/migrate-plan.txt"),
        &out,
    );
    // read-only: no command, nothing asked of Docker that changes anything
    assert_eq!(s.fixture.commands_run().len(), before);
    assert!(!s.fixture.docker_requests().iter().any(|r| !r.starts_with("GET ")));
    // every token is shown by its tail only
    assert!(!out.contains("FAKEresourcesTok01"), "{out}");
}

#[test]
fn plan_as_json_lists_items_with_their_legacy_pieces() {
    let s = server("migrate-plan-json");
    let (code, out) = s.moi(&["migrate", "plan", "--json"]);
    assert_eq!(code, 0, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let names: Vec<&str> = v["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "heal:traefik-homelab",
            "heal:traefik-config-guard",
            "boot",
            "firewall",
            "backup",
            "resources",
            "services",
            "storage",
            "stale",
            "serverstate"
        ]
    );
    let resources = v["items"].as_array().unwrap().iter().find(|i| i["name"] == "resources").unwrap();
    assert_eq!(
        resources["legacy"],
        serde_json::json!([
            { "kind": "cron", "path": "/etc/cron.d/resource-check" },
            { "kind": "file", "path": "/opt/scripts/resource-check.sh" },
            { "kind": "state", "path": "/var/lib/resource-check" },
        ])
    );
}

#[test]
fn without_a_snapshot_the_plan_says_what_to_do() {
    let fixture = Arc::new(
        FixtureHost::open(&common::fixture_dir("cestmoi"))
            .unwrap()
            .with_overlay(&common::scratch("migrate-no-snapshot"))
            .unwrap(),
    );
    let s = Server { host: fixture.clone(), fixture };
    let (code, out) = s.moi(&["migrate", "plan"]);
    assert_eq!(code, 2);
    assert!(out.contains("run `moi snapshot` first"), "{out}");
}

#[test]
fn shadow_holds_every_item_and_takes_over_what_the_legacy_scripts_remembered() {
    let s = server("migrate-shadow");
    let (code, out) = s.moi(&["migrate", "shadow"]);
    assert_eq!(code, 0, "{out}");
    for item in ["resources", "storage", "services", "backup", "boot", "firewall", "heal:traefik-homelab"] {
        assert!(out.contains(&format!("  held      {item}\n")), "{out}");
    }
    assert!(out.contains("imported  resources: CPU counters, restart counts of 2 containers, cpu already high once (from /var/lib/resource-check)"), "{out}");
    assert!(out.contains("imported  heal rule vpn-stack: last acted 2026-11-18T"), "{out}");
    assert!(out.contains("imported  backup: last upload 2026-11-18T"), "{out}");
    // into the places moi's own runs read them from
    assert_eq!(
        s.setting("SELECT value FROM check_values WHERE check_name = 'resources' AND key = 'cpu'"),
        "1000000 800000"
    );
    assert_eq!(
        s.setting("SELECT value FROM check_values WHERE check_name = 'resources' AND key = 'restarts'"),
        r#"{"jellyfin":0,"sonarr":2}"#
    );
    assert_eq!(
        s.setting("SELECT CAST(runs AS TEXT) FROM sustained WHERE check_name = 'resources' AND condition = 'cpu'"),
        "1"
    );
    assert_eq!(s.setting("SELECT CAST(last_run AS TEXT) FROM heal_cooldowns WHERE rule = 'vpn-stack'"), "1795010000");
    assert_eq!(s.setting("SELECT value FROM settings WHERE key = 'offsite.last_upload'"), "1795000000");
    // nothing of the legacy setup was touched
    for path in ["/etc/cron.d/resource-check", "/var/lib/resource-check/cpu", "/opt/scripts/resource-check.sh"] {
        assert!(s.host.exists(Path::new(path)), "{path}");
    }
    assert!(
        !s.fixture.commands_run().iter().any(|c| c.starts_with("systemctl disable") || c.starts_with("systemctl stop"))
    );
}

#[test]
fn a_held_item_is_watched_not_done() {
    let s = server("migrate-held");
    assert_eq!(s.moi(&["migrate", "shadow"]).0, 0);
    let db = s.db();
    assert!(shadow::held(&db, "resources") && shadow::held(&db, "backup") && !shadow::held(&db, "edge"));
    // a check runs and is recorded, and says why nobody heard of it
    let (_, out) = s.moi(&["check", "resources", "--push"]);
    assert!(
        out.contains("not pushed: shadow — the legacy script still reports here (moi migrate switch --only resources)"),
        "{out}"
    );
    // the daemon leaves the backup, the firewall and the legacy units' heal rules alone
    let loaded = config::load(s.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let mut daemon = Daemon::new(s.host.clone(), loaded).unwrap();
    let before = s.fixture.commands_run().len();
    for job in ["backup", "firewall", "heal:traefik-homelab"] {
        assert!(daemon.trigger(job), "{job}");
    }
    let ran = daemon.tick().unwrap();
    for job in ["backup", "firewall", "heal:traefik-homelab"] {
        let (_, outcome) =
            ran.iter().find(|(name, _)| name == job).unwrap_or_else(|| panic!("{job} did not run: {ran:?}"));
        assert!(outcome.message.starts_with("shadow: the legacy setup still does this"), "{job}: {}", outcome.message);
    }
    let commands = s.fixture.commands_run()[before..].to_vec();
    assert!(!commands.iter().any(|c| c.starts_with("restic") || c.contains("tables-restore")), "{commands:?}");
    assert!(!s.fixture.docker_requests().iter().any(|r| r.starts_with("POST /networks/")), "the heal rule acted");
    // released, an item is moi's
    shadow::mark(&db, "resources", true);
    assert!(!shadow::held(&db, "resources"));
    let (_, out) = s.moi(&["check", "resources", "--push"]);
    assert!(!out.contains("shadow"), "{out}");
}

#[test]
fn secrets_the_legacy_scripts_hold_are_taken_into_secrets_toml() {
    let s = server("migrate-secrets");
    // the owner has entered only what the scripts never had
    let own = "[secrets]\nkuma_username = \"FAKEadmin\"\nkuma_password = \"FAKEkumapass\"\nshare_endpoint = \"https://FAKE.example.com\"\nshare_key_id = \"FAKEid\"\nshare_secret = \"FAKEsecret\"\nsonarr_key = \"FAKEentered-by-hand\"\n";
    s.host.write(Path::new("/etc/moi/secrets.toml"), own.as_bytes(), 0o600).unwrap();
    assert_eq!(s.moi(&["config", "check"]).0, 2, "the configuration does not load without them");
    let (code, out) = s.moi(&["migrate", "shadow"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("secrets taken from the legacy scripts into /etc/moi/secrets.toml: restic_repository, restic_password, r2_access_key_id, r2_secret_access_key, cf_account_id, cf_api_token, radarr_key, prowlarr_key"),
        "{out}"
    );
    assert!(!out.contains("FAKEr2secretaccesskey"), "a secret was printed: {out}");
    let written = s.host.read_to_string(Path::new("/etc/moi/secrets.toml")).unwrap();
    assert!(written.starts_with(own), "what was there is unchanged");
    for line in [
        "restic_repository = \"s3:https://fakeaccount.r2.cloudflarestorage.com/fake-bucket\"",
        "restic_password = \"FAKEresticpassword\"",
        "r2_secret_access_key = \"FAKEr2secretaccesskey\"",
        "cf_api_token = \"FAKEcfapitoken\"",
        "radarr_key = \"FAKEradarrapikey0000000000000000\"",
    ] {
        assert!(written.contains(line), "{line} missing in:\n{written}");
    }
    assert_eq!(written.matches("sonarr_key").count(), 1, "a value entered by hand is never replaced");
    assert_eq!(s.host.metadata(Path::new("/etc/moi/secrets.toml")).unwrap().mode, 0o600);
    assert_eq!(s.moi(&["config", "check"]).0, 0);
}

#[test]
fn numbers_are_masked_for_the_same_shape_comparison() {
    assert_eq!(compare::mask_numbers("OK: CPU 12%, RAM 3.5 GB of 16, load 0.42"), "OK: CPU N%, RAM N GB of N, load N");
    assert_eq!(compare::mask_numbers("v1.2.3 at 10.0.0.1."), "vN.N at N.N.");
    assert_eq!(compare::verdict("up: OK: CPU 3%", "up: OK: CPU 3%"), Verdict::Identical);
    assert_eq!(compare::verdict("up: OK: CPU 3%", "up: OK: CPU 41%"), Verdict::SameShape);
    assert_eq!(compare::verdict("up: OK: CPU 3%", "down: OK: CPU 3%"), Verdict::Different);
}

/// Twelve shadow runs of a check five minutes apart, each with a legacy result a few seconds earlier.
fn runs(s: &Server, check: &str, monitor: i64, moi: &dyn Fn(usize) -> String, legacy: &dyn Fn(usize) -> String) {
    let kuma = s.kuma();
    let db = s.db();
    let base = 1_795_800_000_i64;
    for i in 0..12 {
        let at = base + 300 * i64::try_from(i).unwrap();
        db.conn()
            .execute(
                "INSERT INTO check_results (check_name, at, status, message) VALUES (?1, ?2, 'up', ?3)",
                (check, at, moi(i)),
            )
            .unwrap();
        kuma.execute(
            "INSERT INTO heartbeat (monitor_id, status, msg, time) VALUES (?1, 1, ?2, strftime('%Y-%m-%d %H:%M:%f', ?3, 'unixepoch'))",
            (monitor, legacy(i), at - 20),
        )
        .unwrap();
    }
}

#[test]
fn compare_sets_shadow_runs_against_what_kuma_heard_from_the_legacy_scripts() {
    let s = server("migrate-compare");
    assert_eq!(s.moi(&["migrate", "shadow"]).0, 0);
    // resources: the same but for the numbers; storage: one run differs
    runs(&s, "resources", 1, &|i| format!("OK: CPU {i}%, RAM 41%"), &|i| {
        format!("OK: CPU {}%, RAM 41%", if i < 4 { i } else { i + 1 })
    });
    runs(&s, "storage", 3, &|_| "OK: / 41%".to_string(), &|i| {
        if i == 7 { "Problem: / 91% full".to_string() } else { "OK: / 41%".to_string() }
    });
    let (code, out) = s.moi(&["migrate", "compare"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("resources (\"Server Resources\"): 4 identical, 8 same shape, 0 different — ready to switch\n"),
        "{out}"
    );
    assert!(
        out.contains("storage (\"Server Storage\"): 11 identical, 0 same shape, 1 different — NOT the same\n"),
        "{out}"
    );
    assert!(out.contains("    - legacy  up: Problem: / 91% full\n    + moi     up: OK: / 41%\n"), "{out}");
    // a check with nothing to compare yet says so
    assert!(
        out.contains(
            "services (\"Server Internal Services\"): 0 identical, 0 same shape, 0 different — 0 of 12 runs so far\n"
        ),
        "{out}"
    );
    let (_, json) = s.moi(&["migrate", "compare", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["checks"][0]["pairs"].as_array().unwrap().len(), 12);
    assert_eq!(v["checks"][0]["pairs"][0]["verdict"], "same-shape");
}

// ---------------------------------------------------------------------------- switch, rollback, finish

const LEGACY_DIRS: [&str; 8] = [
    "/etc/cron.d",
    "/etc/systemd/system",
    "/opt/scripts",
    "/usr/local/sbin",
    "/etc/host-firewall",
    "/var/lib/resource-check",
    "/var/lib/offsite-backup",
    "/var/tmp",
];

/// Every file under the directories the legacy setup lives in: path → (content, mode).
fn legacy_tree(host: &dyn Host) -> std::collections::BTreeMap<String, (Vec<u8>, u32)> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack: Vec<std::path::PathBuf> = LEGACY_DIRS.iter().map(std::path::PathBuf::from).collect();
    while let Some(at) = stack.pop() {
        match host.symlink_metadata(&at).map(|m| (m.kind, m.mode)) {
            Ok((cestmoi::host::FileKind::Dir, _)) => {
                stack.extend(host.read_dir(&at).unwrap_or_default().into_iter().map(|n| at.join(n)))
            }
            Ok((cestmoi::host::FileKind::Symlink, _)) => {
                out.insert(
                    at.to_string_lossy().into_owned(),
                    (host.read_link(&at).unwrap().to_string_lossy().into_owned().into_bytes(), 0),
                );
            }
            Ok((_, mode)) => {
                out.insert(at.to_string_lossy().into_owned(), (host.read(&at).unwrap(), mode));
            }
            Err(_) => {}
        }
    }
    out
}

/// The fixture's own files cannot be removed: give the server copies that can.
fn removable(s: &Server) {
    let mut stack: Vec<std::path::PathBuf> = LEGACY_DIRS
        .iter()
        .chain(&[
            "/root/.config/restic",
            "/var/lib/serverstate",
            "/etc/serverstate.conf",
            "/usr/local/lib/serverstate",
            "/usr/local/bin/serverstate",
        ])
        .map(std::path::PathBuf::from)
        .collect();
    while let Some(at) = stack.pop() {
        match s.host.symlink_metadata(&at).map(|m| (m.kind, m.mode)) {
            Ok((cestmoi::host::FileKind::Dir, _)) => {
                stack.extend(s.host.read_dir(&at).unwrap_or_default().into_iter().map(|n| at.join(n)))
            }
            Ok((cestmoi::host::FileKind::File, mode)) => s.host.write(&at, &s.host.read(&at).unwrap(), mode).unwrap(),
            _ => {}
        }
    }
}

/// A server in the shadow, with systemd answering.
fn shadowed(test: &str) -> Server {
    let s = server(test);
    removable(&s);
    s.fixture.add_command_then("systemctl ...", 0, "");
    assert_eq!(s.moi(&["migrate", "shadow"]).0, 0);
    s
}

fn systemctl(s: &Server) -> Vec<String> {
    s.fixture.commands_run().into_iter().filter(|c| c.starts_with("systemctl ") && !c.contains(" is-")).collect()
}

#[test]
fn switch_needs_the_shadow_and_a_clean_comparison_for_checks() {
    let s = server("migrate-switch-refused");
    removable(&s);
    s.fixture.add_command_then("systemctl ...", 0, "");
    let (code, out) = s.moi(&["migrate", "switch"]);
    assert_eq!(code, 2);
    assert!(out.contains("run `moi migrate shadow` first"), "{out}");
    assert_eq!(s.moi(&["migrate", "shadow"]).0, 0);
    let (code, out) = s.moi(&["migrate", "switch", "--only", "nothing"]);
    assert_eq!(code, 2);
    assert!(out.contains("no such item: nothing (the plan has: heal:traefik-homelab, "), "{out}");

    // twelve agreeing runs for resources only
    runs(&s, "resources", 1, &|i| format!("OK: CPU {i}%"), &|i| format!("OK: CPU {}%", i + 1));
    let (code, out) = s.moi(&["migrate", "switch", "--only", "resources,storage"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("resources: switched — stopped /etc/cron.d/resource-check\n    reports to the same monitor with the same token\n"), "{out}");
    assert!(
        out.contains("storage: NOT switched — its last 12 shadow runs are not all the same as the legacy script's"),
        "{out}"
    );
    assert!(out.contains("still with the legacy setup: heal:traefik-homelab, "), "{out}");
    assert!(!s.host.exists(Path::new("/etc/cron.d/resource-check")));
    assert!(s.host.exists(Path::new("/etc/cron.d/storage-check")));
    // the monitor the script fed, by the token it used: adopted, not created
    let db = s.db();
    let (name, id, token, created): (String, i64, String, i64) = db
        .conn()
        .query_row("SELECT monitor_name, kuma_id, push_token, created_by_moi FROM monitor_bindings WHERE owner = 'check:resources'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap();
    assert_eq!((name.as_str(), id, token.as_str(), created), ("Server Resources", 1, "FAKEresourcesTok01", 0));
    assert!(!shadow::held(&db, "resources") && shadow::held(&db, "storage"));
    // the script and what it remembered are still there: only its trigger is gone
    assert!(
        s.host.exists(Path::new("/opt/scripts/resource-check.sh"))
            && s.host.exists(Path::new("/var/lib/resource-check/cpu"))
    );
}

#[test]
fn switch_then_rollback_leaves_the_host_as_it_was() {
    let s = shadowed("migrate-switch-rollback");
    let before = legacy_tree(s.host.as_ref());
    assert!(before.len() > 25, "{}", before.len());

    let (code, out) = s.moi(&["migrate", "switch", "--force"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("everything is moi's now"), "{out}");
    for cron in ["resource-check", "service-check", "storage-check"] {
        assert!(!s.host.exists(&Path::new("/etc/cron.d").join(cron)), "{cron}");
    }
    let stopped = systemctl(&s);
    for unit in [
        "traefik-homelab.service",
        "traefik-config-guard.path",
        "homelab-services.service",
        "yams.service",
        "host-firewall.service",
        "server-backup.timer",
        "homelab-selfcheck.timer",
    ] {
        assert!(stopped.contains(&format!("systemctl disable --now {unit}")), "{unit} not stopped: {stopped:?}");
    }
    // the firewall is moi's from the same moment
    assert!(s.fixture.commands_run().iter().any(|c| c.starts_with("iptables-restore --noflush")));
    let db = s.db();
    assert!(shadow::switched(&db).len() >= 9, "{:?}", shadow::switched(&db));
    // every piece has a copy, and the log says what happened
    assert_eq!(
        s.host.read(Path::new("/var/lib/moi/archive/legacy/etc/cron.d/resource-check")).unwrap(),
        before["/etc/cron.d/resource-check"].0
    );
    assert!(s.host.exists(Path::new("/var/lib/moi/archive/legacy/var/lib/resource-check/cpu")));
    let log = s.host.read_to_string(Path::new("/var/lib/moi/archive/migration.log")).unwrap();
    assert!(log.contains(" switch resources: stopped /etc/cron.d/resource-check\n"), "{log}");

    let at = s.fixture.commands_run().len();
    let (code, out) = s.moi(&["migrate", "rollback"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("resources: rolled back — back: /etc/cron.d/resource-check"), "{out}");
    assert_eq!(legacy_tree(s.host.as_ref()), before, "files, units and cron are what they were");
    let started: Vec<String> =
        s.fixture.commands_run()[at..].iter().filter(|c| c.starts_with("systemctl enable")).cloned().collect();
    for unit in ["traefik-homelab.service", "host-firewall.service", "server-backup.timer", "yams.service"] {
        assert!(started.contains(&format!("systemctl enable --now {unit}")), "{unit} not started again: {started:?}");
    }
    let db = s.db();
    assert!(shadow::switched(&db).is_empty() && shadow::held(&db, "resources") && shadow::held(&db, "firewall"));
    let bound: i64 = db.conn().query_row("SELECT COUNT(*) FROM monitor_bindings", [], |r| r.get(0)).unwrap();
    assert_eq!(bound, 0, "moi let go of the monitors");
    let (_, out) = s.moi(&["check", "resources", "--push"]);
    assert!(out.contains("not pushed: shadow"), "{out}");
}

#[test]
fn a_binding_that_existed_before_the_switch_survives_the_rollback() {
    let s = shadowed("migrate-rollback-binding");
    s.db()
        .conn()
        .execute(
            "INSERT INTO monitor_bindings (owner, monitor_name, kuma_id, push_token, created_by_moi, updated_at) VALUES ('check:storage', 'Server Storage', 3, 'FAKEstorageTok0003', 0, 0)",
            [],
        )
        .unwrap();
    assert_eq!(s.moi(&["migrate", "switch", "--force", "--only", "storage"]).0, 0);
    assert_eq!(s.moi(&["migrate", "rollback", "--only", "storage"]).0, 0);
    let token: String = s
        .db()
        .conn()
        .query_row("SELECT push_token FROM monitor_bindings WHERE owner = 'check:storage'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(token, "FAKEstorageTok0003");
    let (code, out) = s.moi(&["migrate", "rollback", "--only", "storage"]);
    assert_eq!(code, 2);
    assert!(out.contains("storage is not switched"), "{out}");
}

#[test]
fn finish_waits_for_everything_then_removes_what_is_in_the_archive() {
    let s = shadowed("migrate-finish");
    let before = legacy_tree(s.host.as_ref());
    let (code, out) = s.moi(&["migrate", "finish"]);
    assert_eq!(code, 2);
    assert!(out.contains("not everything is switched yet: heal:traefik-homelab, "), "{out}");
    assert_eq!(s.moi(&["migrate", "switch", "--force"]).0, 0);
    let (code, out) = s.moi(&["migrate", "finish"]);
    assert_eq!(code, 2);
    assert!(out.contains("the last item was switched 0 days ago"), "{out}");
    let (code, out) = s.moi(&["migrate", "finish", "--now"]);
    assert_eq!(code, 2);
    assert!(out.contains("moi has not run a backup since the switch"), "{out}");
    assert_eq!(legacy_tree(s.host.as_ref()).len(), before.len() - 3, "nothing but the cron files is gone so far");

    // a week later, with a backup that worked
    s.fixture.advance(std::time::Duration::from_secs(8 * 86_400));
    let now = cestmoi::host::unix_seconds(s.host.now());
    s.db().conn().execute("INSERT INTO backup_runs (at, status) VALUES (?1, 'ok')", [now - 3600]).unwrap();
    s.fixture.add_command("/usr/local/bin/serverstate uninstall --purge --yes", 0, "");
    let (code, out) = s.moi(&["migrate", "finish"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("legacy files, units and directories removed ("), "{out}");
    assert!(out.contains("serverstate: removed by its own uninstaller"), "{out}");
    assert!(out.contains("snapshot taken"), "{out}");
    // gone from the server, every one of them in the archive
    for (path, (content, _)) in &before {
        let legacy = !path.ends_with("/README.md")
            && !path.contains("/lib/")
            && !path.contains(".wants/")
            && !path.contains("docker.service.d")
            && !path.contains("snap-core22");
        if legacy {
            assert!(!s.host.exists(Path::new(path)), "{path} is still there");
            let copy = Path::new("/var/lib/moi/archive/legacy").join(path.trim_start_matches('/'));
            assert_eq!(&s.host.read(&copy).unwrap_or_else(|e| panic!("{path} has no copy: {e}")), content, "{path}");
        }
    }
    for path in ["/var/lib/serverstate", "/etc/serverstate.conf", "/root/.config/restic/r2.env"] {
        assert!(!s.host.exists(Path::new(path)), "{path}");
    }
    // what was never the legacy setup's is untouched
    assert!(
        s.host.exists(Path::new("/opt/scripts/lib/common.sh"))
            && s.host.exists(Path::new("/etc/systemd/system/docker.service.d/storage.conf"))
    );
    // the migration is over
    assert!(!shadow::active(&s.db()));
    let (_, out) = s.moi(&["migrate", "plan"]);
    assert!(out.starts_with("nothing to migrate"), "{out}");
    let (_, out) = s.moi(&["check", "resources", "--push"]);
    assert!(!out.contains("shadow"), "{out}");
}

#[test]
fn the_prototypes_history_comes_before_mois_own() {
    use cestmoi::state::history::History;
    let s = server("migrate-history");
    s.fixture.add_command_then("systemctl ...", 0, "");
    // the prototype's repository: two commits, a year ago
    let proto = StateDir::new("/var/lib/proto");
    let write = |path: &str, text: &str| {
        let path = proto.state().join(path);
        s.host.create_dir_all(path.parent().unwrap(), 0o700).unwrap();
        s.host.write(&path, text.as_bytes(), 0o600).unwrap();
    };
    let old = s.host.now() - std::time::Duration::from_secs(365 * 86_400);
    let now = s.host.now();
    s.fixture.set_now(old);
    write("snapshot/packages/apt-manual.txt", "bash\n");
    write("NOTES.md", "# Notes\n\nthe disk in bay 2 is the old one\n");
    write("CONTEXT.md", "generated, never part of the history\n");
    let history = History::open(s.host.as_ref(), &proto).unwrap();
    history.commit(s.host.as_ref(), &proto).unwrap().unwrap();
    s.fixture.set_now(old + std::time::Duration::from_secs(86_400));
    write("snapshot/packages/apt-manual.txt", "bash\nrestic\n");
    history.commit(s.host.as_ref(), &proto).unwrap().unwrap();
    drop(history);
    s.fixture.set_now(now);
    let overlay = common::scratch_path("migrate-history");
    std::fs::rename(overlay.join("var/lib/proto/state"), overlay.join("var/lib/serverstate")).unwrap();

    let (_, own) = s.moi(&["log"]);
    let own_commits = own.lines().filter(|l| !l.starts_with(' ') && !l.is_empty()).count();
    assert_eq!(s.moi(&["migrate", "shadow"]).0, 0);
    let (code, out) = s.moi(&["migrate", "switch", "--only", "serverstate"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("2 commits of its history now come before moi's (`moi log`)"), "{out}");

    let (_, log) = s.moi(&["log", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&log).unwrap();
    let entries = v["commits"].as_array().unwrap_or_else(|| panic!("{log}"));
    assert_eq!(entries.len(), own_commits + 2, "{log}");
    // newest first: moi's own on top, the prototype's two at the bottom with their dates
    let oldest = &entries[entries.len() - 1];
    assert!(oldest["subject"].as_str().unwrap().starts_with("initial snapshot of"), "{oldest}");
    assert_eq!(oldest["time"].as_str().unwrap(), cestmoi::host::format_rfc3339(old));
    assert!(entries[entries.len() - 2]["subject"].as_str().unwrap().starts_with("changed: "), "{log}");
    // what an old commit held can be shown
    let (code, shown) = s.moi(&["diff", &format!("HEAD~{}", entries.len() - 2)]);
    assert_eq!(code, 0, "{shown}");
    assert!(shown.contains("restic"), "{shown}");
    // the notes are kept, under a heading of their own
    let notes = s.host.read_to_string(Path::new("/var/lib/moi/state/NOTES.md")).unwrap();
    assert!(
        notes.contains("## Notes from serverstate") && notes.contains("the disk in bay 2 is the old one"),
        "{notes}"
    );
    // once
    assert_eq!(s.moi(&["migrate", "rollback", "--only", "serverstate"]).0, 0);
    assert_eq!(s.moi(&["migrate", "switch", "--only", "serverstate"]).0, 0);
    let (_, again) = s.moi(&["log", "--json"]);
    let again: serde_json::Value = serde_json::from_str(&again).unwrap();
    assert_eq!(again["commits"].as_array().unwrap().len(), entries.len());
}
