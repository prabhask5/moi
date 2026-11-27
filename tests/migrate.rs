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
