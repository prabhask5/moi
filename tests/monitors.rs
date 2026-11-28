//! Uptime Kuma inventory, desired monitors and the adoption planner (docs/spec/06-monitors.md).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cestmoi::cli;
use cestmoi::config::{self, Config, Paths};
use cestmoi::docker::Docker;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::monitors::inventory::{self, DockerHost};
use cestmoi::monitors::{self, Action, Binding, Desired, Feeder, Inventory, Kind, KumaMonitor, Options, Plan};
use cestmoi::state::StateDir;

const KUMA_DB: &str = "/opt/services/monitoring/uptime-kuma/data/kuma.db";

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
    fn config(&self) -> Config {
        config::load(self.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap().config
    }

    fn edit_config(&self, edit: impl FnOnce(String) -> String) {
        let path = Path::new("/etc/moi/moi.toml");
        let text = edit(self.host.read_to_string(path).unwrap());
        self.host.write(path, text.as_bytes(), 0o644).unwrap();
    }

    /// (exit code, stdout or the error) of an in-process `moi …`.
    fn moi(&self, args: &[&str]) -> (u8, String) {
        let mut out = Vec::new();
        match cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out) {
            Ok(()) => (0, String::from_utf8(out).unwrap()),
            Err(e) => (e.exit_code(), e.to_string()),
        }
    }
}

// ------------------------------------------------------------------ inventory

#[test]
fn the_inventory_is_read_from_kumas_database_found_through_its_container() {
    let s = server("monitors-inventory");
    let docker = Docker::new(s.host.clone());
    assert_eq!(inventory::locate(s.host.as_ref(), &docker).as_deref(), Some(KUMA_DB));
    let found = inventory::read(s.host.as_ref(), KUMA_DB).unwrap();
    assert_eq!(found.monitors.len(), 21);
    let resources = &found.monitors[0];
    assert_eq!(
        (resources.id, resources.name.as_str(), resources.kind.as_str(), resources.active),
        (1, "Server Resources", "push", true)
    );
    assert_eq!((resources.interval, resources.push_token.as_deref()), (Some(360), Some("FAKEresourcesTok01")));
    assert_eq!(resources.notifications, ["Email"]);
    assert_eq!(resources.last_status.as_deref(), Some("up"));
    let devbox = found.monitors.iter().find(|m| m.name == "devbox").unwrap();
    assert!(!devbox.active, "paused");
    assert_eq!(
        found.monitors.iter().filter(|m| m.is_watchdog()).map(|m| m.name.as_str()).collect::<Vec<_>>(),
        ["healthchecks.io watchdog"]
    );
    assert_eq!(found.notifications, [(1, "Email".to_string())]);
    assert!(found.tags.iter().any(|(_, name)| name == "public"));
    // this Kuma has no docker hosts table at all: that is "none", not an error
    assert_eq!(found.docker_hosts, Vec::<DockerHost>::new());
}

#[test]
fn reading_never_changes_the_database_or_leaves_files_next_to_it() {
    let s = server("monitors-read-only");
    let real = common::fixture_dir("cestmoi").join("root").join(KUMA_DB.trim_start_matches('/'));
    let before = std::fs::read(&real).unwrap();
    let listing = |dir: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        names
    };
    let files = listing(real.parent().unwrap());
    inventory::read(s.host.as_ref(), KUMA_DB).unwrap();
    assert_eq!(s.moi(&["monitors", "sync", "--dry-run"]).0, 0);
    assert!(std::fs::read(&real).unwrap() == before);
    assert_eq!(listing(real.parent().unwrap()), files, "no -wal, -shm or journal files");
    assert!(!s.overlay.join(KUMA_DB.trim_start_matches('/')).exists(), "and no copy in the overlay");
}

/// A database with whatever schema a test wants, in the overlay.
fn kuma_db(s: &Server, sql: &str) -> String {
    let dir = s.overlay.join("srv/kuma");
    std::fs::create_dir_all(&dir).unwrap();
    let conn = rusqlite::Connection::open(dir.join("kuma.db")).unwrap();
    conn.execute_batch(sql).unwrap();
    "/srv/kuma/kuma.db".to_string()
}

#[test]
fn columns_and_tables_another_kuma_version_lacks_are_simply_absent() {
    let s = server("monitors-schema");
    // no push_token, docker_*, parent or maxretries columns; no notification, tag or heartbeat tables
    let db = kuma_db(
        &s,
        "CREATE TABLE monitor (id INTEGER PRIMARY KEY, name TEXT, type TEXT, active BOOLEAN, interval INTEGER, url TEXT);
         INSERT INTO monitor VALUES (7, 'Site', 'http', 1, 60, 'https://example.com/'), (8, 'Paused', 'push', 0, 300, NULL);",
    );
    let found = inventory::read(s.host.as_ref(), &db).unwrap();
    assert_eq!(found.monitors.len(), 2);
    let site = &found.monitors[0];
    assert_eq!(
        (site.name.as_str(), site.url.as_deref(), site.push_token.as_deref(), site.parent, site.last_status.as_deref()),
        ("Site", Some("https://example.com/"), None, None, None)
    );
    assert!(!found.monitors[1].active);
    assert!(found.notifications.is_empty() && found.tags.is_empty() && found.docker_hosts.is_empty());

    // a 2.x-style database: docker hosts, tags, heartbeats
    let db = kuma_db(
        &s,
        "DROP TABLE monitor;
         CREATE TABLE monitor (id INTEGER PRIMARY KEY, name TEXT, type TEXT, active BOOLEAN, interval INTEGER, maxretries INTEGER, push_token TEXT, docker_container TEXT, docker_host INTEGER, url TEXT, parent INTEGER, conditions TEXT);
         CREATE TABLE docker_host (id INTEGER PRIMARY KEY, user_id INT, docker_daemon TEXT, docker_type TEXT, name TEXT);
         CREATE TABLE tag (id INTEGER PRIMARY KEY, name TEXT, color TEXT);
         CREATE TABLE monitor_tag (id INTEGER PRIMARY KEY, monitor_id INT, tag_id INT, value TEXT);
         CREATE TABLE heartbeat (id INTEGER PRIMARY KEY, monitor_id INT, status INT, msg TEXT, time DATETIME);
         INSERT INTO docker_host VALUES (1, 1, '/var/run/docker.sock', 'socket', 'local');
         INSERT INTO tag VALUES (1, 'moi', '#000');
         INSERT INTO monitor VALUES (3, 'box: web', 'docker', 1, 60, 1, NULL, 'web', 1, NULL, 2, '[]');
         INSERT INTO monitor_tag VALUES (1, 3, 1, '');
         INSERT INTO heartbeat VALUES (1, 3, 1, 'running', '2026-11-26 16:00:00'), (2, 3, 0, 'exited', '2026-11-26 16:59:00');",
    );
    let found = inventory::read(s.host.as_ref(), &db).unwrap();
    let web = &found.monitors[0];
    assert_eq!(
        (web.docker_container.as_deref(), web.docker_host, web.parent, web.retries),
        (Some("web"), Some(1), Some(2), Some(1))
    );
    assert_eq!(web.tags, ["moi"]);
    assert_eq!(web.last_status.as_deref(), Some("down"), "the latest heartbeat");
    assert_eq!(found.docker_hosts, [DockerHost { id: 1, name: "local".into(), daemon: "/var/run/docker.sock".into() }]);
    // not a Kuma database
    let other = s.overlay.join("srv/other.db");
    rusqlite::Connection::open(&other).unwrap().execute_batch("CREATE TABLE t (x)").unwrap();
    assert!(inventory::read(s.host.as_ref(), "/srv/other.db").is_none());
    assert!(inventory::read(s.host.as_ref(), "/srv/missing.db").is_none());
}

// ------------------------------------------------------------------ what moi wants

#[test]
fn desired_monitors_follow_the_config() {
    let s = server("monitors-desired");
    s.edit_config(|t| t.replace("[boot]\n", "[boot]\nverify_monitor = \"Server Boot\"\n"));
    let wanted = monitors::desired(&s.config(), &Docker::new(s.host.clone()));
    let brief: Vec<(&str, &str, Kind, u64)> =
        wanted.iter().take(6).map(|d| (d.owner.as_str(), d.name.as_str(), d.kind, d.interval)).collect();
    assert_eq!(
        brief,
        [
            ("check:resources", "Server Resources", Kind::Push, 360),
            ("check:storage", "Server Storage", Kind::Push, 360),
            ("check:services", "Server Internal Services", Kind::Push, 360),
            ("check:backup", "Server Backup", Kind::Push, 90_000),
            ("freshness:snapshot", "cestmoi: snapshot", Kind::Push, 93_600),
            ("boot:verify", "Server Boot", Kind::Push, 2_592_000),
        ]
    );
    // one docker monitor per container that is meant to keep running; swarm tasks belong to their service
    let containers: Vec<&Desired> = wanted.iter().filter(|d| d.kind == Kind::Docker).collect();
    assert_eq!(containers.len(), 35);
    assert!(
        containers
            .iter()
            .all(|d| d.interval == 60 && d.owner == format!("container:{}", d.container.as_deref().unwrap()))
    );
    assert!(containers.iter().any(|d| d.name == "cestmoi: jellyfin"));
    assert!(!wanted.iter().any(|d| d.name.contains("dokploy.1.")), "no monitor for a swarm task");

    // switches
    s.edit_config(|t| {
        t.replace("containers = true", "containers = false").replace("freshness = true", "freshness = false")
    });
    let wanted = monitors::desired(&s.config(), &Docker::new(s.host.clone()));
    assert_eq!(
        wanted.iter().map(|d| d.owner.as_str()).collect::<Vec<_>>(),
        ["check:resources", "check:storage", "check:services", "check:backup", "boot:verify"]
    );
    // a check without a monitor has none; a custom check with one has
    s.edit_config(|t| format!("{}\n[[checks.custom]]\nname = \"certs\"\nrun = \"/bin/true\"\ninterval = \"1h\"\nmonitor = \"Certificates\"\n", t.replace("monitor = \"Server Storage\"\n", "")));
    let wanted = monitors::desired(&s.config(), &Docker::new(s.host.clone()));
    assert!(!wanted.iter().any(|d| d.owner == "check:storage"));
    let certs = wanted.iter().find(|d| d.owner == "check:certs").unwrap();
    assert_eq!((certs.name.as_str(), certs.interval), ("Certificates", 4320));
}

#[test]
fn a_container_without_a_restart_policy_gets_no_monitor() {
    let s = server("monitors-desired-restart");
    let list: serde_json::Value = serde_json::from_slice(
        &std::fs::read(common::fixture_dir("cestmoi").join("docker/containers/json.json")).unwrap(),
    )
    .unwrap();
    let id = list.as_array().unwrap().iter().find(|c| c["Names"][0] == "/watchtower").unwrap()["Id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut watchtower: serde_json::Value = serde_json::from_slice(
        &std::fs::read(common::fixture_dir("cestmoi").join(format!("docker/containers/{id}/json.json"))).unwrap(),
    )
    .unwrap();
    watchtower["HostConfig"]["RestartPolicy"]["Name"] = "no".into();
    s.fixture.set_docker(&format!("/containers/{id}/json"), &watchtower.to_string());
    let wanted = monitors::desired(&s.config(), &Docker::new(s.host.clone()));
    assert!(!wanted.iter().any(|d| d.owner == "container:watchtower"));
    assert_eq!(wanted.iter().filter(|d| d.kind == Kind::Docker).count(), 34);
}

// ------------------------------------------------------------------ the planner

fn push(owner: &str, name: &str, interval: u64) -> Desired {
    Desired {
        owner: owner.into(),
        name: name.into(),
        kind: Kind::Push,
        interval,
        retries: 1,
        container: None,
        url: None,
    }
}

fn container(name: &str) -> Desired {
    Desired {
        owner: format!("container:{name}"),
        name: format!("box: {name}"),
        kind: Kind::Docker,
        interval: 60,
        retries: 1,
        container: Some(name.into()),
        url: None,
    }
}

fn kuma(id: i64, name: &str, kind: &str) -> KumaMonitor {
    KumaMonitor { id, name: name.into(), kind: kind.into(), active: true, interval: Some(60), ..KumaMonitor::default() }
}

fn push_monitor(id: i64, name: &str, token: &str, interval: i64) -> KumaMonitor {
    KumaMonitor { push_token: Some(token.into()), interval: Some(interval), ..kuma(id, name, "push") }
}

fn bound(owner: &str, id: i64, mine: bool) -> Binding {
    Binding {
        owner: owner.into(),
        monitor_name: String::new(),
        kuma_id: Some(id),
        push_token: None,
        created_by_moi: mine,
    }
}

fn plan_of(
    config: &Config,
    desired: &[Desired],
    monitors: Vec<KumaMonitor>,
    bindings: &[Binding],
    feeders: &[Feeder],
    options: Options,
) -> Plan {
    let inventory = Inventory { monitors, notifications: vec![(1, "Email".into())], ..Inventory::default() };
    monitors::plan(config, desired, &inventory, bindings, feeders, options)
}

/// (action, monitor, detail) per step.
fn brief(plan: &Plan) -> Vec<(Action, &str, &str)> {
    plan.steps.iter().map(|s| (s.action, s.monitor.as_str(), s.detail.as_str())).collect()
}

#[test]
fn the_first_matching_rule_decides_what_happens_to_a_desired_monitor() {
    let s = server("monitors-plan-rules");
    let config = s.config();
    let desired = [
        push("check:resources", "Server Resources", 360),
        push("check:storage", "Server Storage", 360),
        push("check:new", "Brand New", 360),
        container("web"),
        container("db"),
    ];
    let existing = vec![
        // fed by the legacy script under another name: only the token says it is the one
        push_monitor(1, "Resources (old name)", "FAKEtokenAAAA1111", 360),
        // a decoy with the wanted name and another token
        push_monitor(2, "Server Resources", "FAKEtokenBBBB2222", 360),
        push_monitor(3, "Server Storage", "FAKEtokenCCCC3333", 360),
        KumaMonitor { docker_container: Some("web".into()), ..kuma(4, "Web container", "docker") },
    ];
    let feeders = [Feeder {
        owner: "check:resources".into(),
        token: "FAKEtokenAAAA1111".into(),
        file: "/opt/scripts/resource-check.sh".into(),
    }];
    let plan = plan_of(&config, &desired, existing, &[], &feeders, Options::default());
    assert_eq!(
        brief(&plan),
        [
            (
                Action::Adopt,
                "Resources (old name)",
                "token …1111 (fed by /opt/scripts/resource-check.sh — migration map)"
            ),
            (Action::Adopt, "Server Storage", "token …3333"),
            (Action::Create, "Brand New", "interval 360s"),
            (Action::Adopt, "Web container", "watches container web"),
            (Action::Create, "box: db", "host local"),
            (Action::Keep, "Server Resources", "not managed by moi"),
        ]
    );
    // what an adoption binds: the monitor's id and its own token, never a new one
    let adopted = &plan.steps[0];
    assert_eq!(
        (adopted.kuma_id, adopted.token.as_deref(), adopted.owner.as_deref()),
        (Some(1), Some("FAKEtokenAAAA1111"), Some("check:resources"))
    );
    assert_eq!(plan.changes(), 5);
    // the whole token is never in what is printed or serialized
    let shown = format!("{}{}", plan.render(), serde_json::to_string(&plan).unwrap());
    assert!(!shown.contains("FAKEtokenAAAA1111") && !shown.contains("FAKEtokenCCCC3333"), "{shown}");
}

#[test]
fn a_binding_wins_over_everything_and_a_binding_to_a_deleted_monitor_is_forgotten() {
    let s = server("monitors-plan-binding");
    let config = s.config();
    let desired = [push("check:resources", "Server Resources", 360)];
    let existing = vec![
        push_monitor(1, "Renamed by the owner", "FAKEtokenAAAA1111", 360),
        push_monitor(2, "Server Resources", "FAKEtokenBBBB2222", 360),
    ];
    let plan =
        plan_of(&config, &desired, existing.clone(), &[bound("check:resources", 1, false)], &[], Options::default());
    assert_eq!(
        brief(&plan),
        [
            (Action::Keep, "Renamed by the owner", "bound to check:resources"),
            (Action::Keep, "Server Resources", "not managed by moi")
        ]
    );
    assert_eq!(plan.changes(), 0, "a second sync is a no-op");
    // the bound monitor was deleted in Kuma: fall back to the other rules
    let plan = plan_of(&config, &desired, existing, &[bound("check:resources", 99, false)], &[], Options::default());
    assert_eq!(brief(&plan)[0], (Action::Adopt, "Server Resources", "token …2222"));
}

#[test]
fn a_name_taken_by_another_type_is_a_conflict_and_nothing_is_done() {
    let s = server("monitors-plan-conflict");
    let plan = plan_of(
        &s.config(),
        &[push("check:resources", "Server Resources", 360)],
        vec![kuma(5, "Server Resources", "http")],
        &[],
        &[],
        Options::default(),
    );
    assert_eq!(
        brief(&plan),
        [(Action::Conflict, "Server Resources", "exists as a http monitor, moi wants push: rename one of them")]
    );
    assert_eq!(plan.changes(), 0);
}

#[test]
fn differences_from_mois_defaults_are_drift_and_only_change_with_update() {
    let s = server("monitors-plan-drift");
    let config = s.config();
    let desired = [push("check:backup", "Server Backup", 90_000)];
    let slow = KumaMonitor { active: false, ..push_monitor(4, "Server Backup", "FAKEtokenDDDD4444", 86_400) };
    let plan = plan_of(&config, &desired, vec![slow.clone()], &[], &[], Options::default());
    assert_eq!(
        brief(&plan),
        [
            (Action::Adopt, "Server Backup", "token …4444"),
            (Action::Drift, "Server Backup", "interval 86400 (moi default 90000), paused")
        ]
    );
    assert_eq!(plan.changes(), 1, "drift alone changes nothing");
    let plan = plan_of(&config, &desired, vec![slow], &[], &[], Options { update: true, prune: false });
    assert_eq!(plan.steps[1].action, Action::Update);
    // same settings: no drift line
    let plan = plan_of(
        &config,
        &desired,
        vec![push_monitor(4, "Server Backup", "FAKEtokenDDDD4444", 90_000)],
        &[],
        &[],
        Options::default(),
    );
    assert_eq!(plan.steps.len(), 1);
}

#[test]
fn only_monitors_moi_created_are_ever_paused_or_deleted() {
    let s = server("monitors-plan-orphans");
    let config = s.config();
    let tagged = |id, name: &str| KumaMonitor {
        tags: vec!["moi".into()],
        docker_container: Some("gone".into()),
        ..kuma(id, name, "docker")
    };
    let existing = vec![
        tagged(10, "box: gone"),
        // created by a human who happened to use the tag: no binding, so not moi's
        tagged(11, "box: tagged by hand"),
        // adopted, not created: the binding exists but moi does not own the monitor
        kuma(12, "Adopted long ago", "push"),
        // moi's own, already paused
        KumaMonitor { active: false, ..tagged(13, "box: paused") },
    ];
    let bindings =
        [bound("container:gone", 10, true), bound("check:old", 12, false), bound("container:paused", 13, true)];
    let plan = plan_of(&config, &[], existing.clone(), &bindings, &[], Options::default());
    assert_eq!(
        brief(&plan),
        [
            (Action::Pause, "box: gone", "container:gone is gone"),
            (Action::Keep, "box: tagged by hand", "not managed by moi"),
            (Action::Keep, "Adopted long ago", "not managed by moi"),
        ]
    );
    let plan = plan_of(&config, &[], existing, &bindings, &[], Options { update: false, prune: true });
    let deleted: Vec<&str> =
        plan.steps.iter().filter(|s| s.action == Action::Delete).map(|s| s.monitor.as_str()).collect();
    assert_eq!(deleted, ["box: gone", "box: paused"]);
    assert!(plan.steps.iter().filter(|s| s.action != Action::Delete).all(|s| s.action == Action::Keep));
}

#[test]
fn a_missing_watchdog_or_notification_is_a_warning() {
    let s = server("monitors-plan-warnings");
    s.edit_config(|t| t.replace("notifications = [\"Email\"]", "notifications = [\"Email\", \"Pager\"]"));
    let config = s.config();
    let watchdog =
        KumaMonitor { url: Some("https://hc-ping.com/FAKEpingkey0000000000/x".into()), ..kuma(1, "hc", "http") };
    let plan = plan_of(&config, &[], vec![watchdog.clone()], &[], &[], Options::default());
    assert_eq!(
        plan.warnings,
        ["notification \"Pager\" does not exist in Uptime Kuma: created monitors would alert nobody"]
    );
    assert_eq!(brief(&plan), [(Action::Keep, "hc", "not managed by moi (watchdog)")]);
    // paused or absent: the chain is broken
    for existing in [vec![KumaMonitor { active: false, ..watchdog }], vec![]] {
        let plan = plan_of(&config, &[], existing, &[], &[], Options::default());
        assert!(plan.warnings[0].starts_with("no active healthchecks.io watchdog monitor"), "{:?}", plan.warnings);
    }
}

// ------------------------------------------------------------------ the commands

#[test]
fn dry_run_prints_the_plan_for_cestmoi_without_credentials_or_network() {
    let s = server("monitors-dry-run");
    // moi has no Kuma login at all: the plan must not need one
    s.edit_config(|t| {
        t.replace("username = \"secret:kuma_username\"\n", "").replace("password = \"secret:kuma_password\"\n", "")
    });
    assert!(s.config().monitors.kuma.username.is_none() && s.config().monitors.kuma.password.is_none());
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    let (code, out) = s.moi(&["monitors", "sync", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    common::assert_golden(
        &common::fixture_dir("cestmoi").parent().unwrap().parent().unwrap().join("expected/cestmoi/monitors-plan.txt"),
        &out,
    );
    assert!(!out.contains("FAKEresourcesTok01"), "tokens are shown by their last characters only");
    assert!(!out.contains("FAKEpingkey"), "the healthchecks.io key is never printed");
    assert!(s.fixture.docker_requests().iter().all(|r| r.starts_with("GET ")), "nothing was changed in Docker");
    // nothing was bound: a dry run is a dry run
    let state = StateDir::new("/var/lib/moi");
    assert_eq!(monitors::bindings(&state.open_db(s.host.as_ref()).unwrap()).unwrap(), []);

    let (_, json) = s.moi(&["monitors", "sync", "--dry-run", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!((v["ok"].as_bool(), v["dry_run"].as_bool(), v["changes"].as_u64()), (Some(true), Some(true), Some(40)));
    assert_eq!(
        v["plan"][0],
        serde_json::json!({ "action": "adopt", "monitor": "Server Resources", "kind": "push", "detail": "token …ok01 (fed by /opt/scripts/resource-check.sh — migration map)", "owner": "check:resources", "kuma_id": 1 })
    );
}

#[test]
fn list_shows_every_monitor_with_its_state_and_owner() {
    let s = server("monitors-list");
    let (code, out) = s.moi(&["monitors", "list"]);
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 22);
    assert_eq!(lines[0], "MONITOR                   TYPE     STATUS  OWNER");
    assert_eq!(
        lines[1],
        "Server Resources          push     up      check:resources (not bound yet: moi monitors sync)"
    );
    assert!(lines.contains(&"devbox                    ping     paused  -"), "{out}");
    assert!(lines.contains(&"healthchecks.io watchdog  http     up      - (watchdog)"), "{out}");
    // bound monitors show their owner plainly
    let state = StateDir::new("/var/lib/moi");
    state.open_db(s.host.as_ref()).unwrap().conn().execute("INSERT INTO monitor_bindings (owner, monitor_name, kuma_id, updated_at) VALUES ('check:resources', 'Server Resources', 1, 0)", []).unwrap();
    let (_, out) = s.moi(&["monitors", "list"]);
    assert_eq!(out.lines().nth(1).unwrap(), "Server Resources          push     up      check:resources");
    let (_, json) = s.moi(&["monitors", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["db"], KUMA_DB);
    assert_eq!(
        v["monitors"][0],
        serde_json::json!({ "id": 1, "name": "Server Resources", "type": "push", "status": "up", "owner": "check:resources", "bound": true, "watchdog": false })
    );
}

#[test]
fn errors_say_what_to_do() {
    // no [monitors.kuma]
    let overlay = common::scratch("monitors-unconfigured");
    let host: Arc<dyn Host> =
        Arc::new(FixtureHost::open(&common::fixture_dir("minimal")).unwrap().with_overlay(&overlay).unwrap());
    let run = |host: &Arc<dyn Host>, args: &[&str]| {
        cli::execute(&cli::parse(args).unwrap(), host.clone(), &mut Vec::new()).unwrap_err()
    };
    let err = run(&host, &["monitors", "sync", "--dry-run"]);
    assert_eq!(
        (err.exit_code(), err.to_string().as_str()),
        (2, "monitors are not configured: add a [monitors.kuma] section with the url of your Uptime Kuma to moi.toml")
    );
    // configured, but no Kuma container on this machine
    let fixture = FixtureHost::open(&common::fixture_dir("minimal"))
        .unwrap()
        .with_overlay(&common::scratch("monitors-no-kuma"))
        .unwrap();
    common::write_config(&fixture, "[monitors.kuma]\nurl = \"http://kuma.example\"\n", None);
    let host: Arc<dyn Host> = Arc::new(fixture);
    let err = run(&host, &["monitors", "list"]);
    assert!(
        err.to_string().starts_with("cannot find Uptime Kuma's database: no container of an uptime-kuma image"),
        "{err}"
    );
}
