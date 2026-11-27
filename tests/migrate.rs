//! `moi migrate`: from legacy scripts, cron jobs and units to moi (docs/spec/11-migration.md).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use cestmoi::cli;
use cestmoi::host::{FixtureHost, Host};

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
