//! `moi monitors sync` against Uptime Kuma's socket API (docs/spec/06-monitors.md): locally against an
//! in-memory stand-in, in the CI integration job against real Kuma servers (the ignored tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cestmoi::cli;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::monitors::kuma::Kuma;
use cestmoi::monitors::socket::Session;
use cestmoi::monitors::{self, inventory};
use cestmoi::state::StateDir;
use common::fake_kuma::FakeKuma;
use serde_json::{Value, json};

const USER: &str = "FAKE-kuma_username-value";
const PASSWORD: &str = "FAKE-kuma_password-value";
const KUMA_DB: &str = "/opt/services/monitoring/uptime-kuma/data/kuma.db";

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
    kuma: FakeKuma,
}

/// The cestmoi fixture with a fake Kuma that has what the fixture's `kuma.db` has.
fn server(test: &str, version: &str) -> Server {
    let overlay: PathBuf = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    let kuma = FakeKuma::new(version, USER, PASSWORD);
    kuma.seed(&inventory::read(fixture.as_ref(), KUMA_DB).unwrap());
    kuma.serve(&fixture);
    Server { host: fixture.clone(), fixture, kuma }
}

impl Server {
    fn edit_config(&self, edit: impl FnOnce(String) -> String) {
        let path = Path::new("/etc/moi/moi.toml");
        let text = edit(self.host.read_to_string(path).unwrap());
        self.host.write(path, text.as_bytes(), 0o644).unwrap();
    }

    /// (exit code, stdout or the error) of an in-process `moi …`.
    fn moi(&self, args: &[&str]) -> (u8, String) {
        moi(&self.host, args)
    }

    fn bindings(&self) -> Vec<monitors::Binding> {
        monitors::bindings(&StateDir::new("/var/lib/moi").open_db(self.host.as_ref()).unwrap()).unwrap()
    }

    /// Take a container off the machine.
    fn remove_container(&self, name: &str) {
        let list: Value = serde_json::from_slice(
            &std::fs::read(common::fixture_dir("cestmoi").join("docker/containers/json.json")).unwrap(),
        )
        .unwrap();
        let kept: Vec<&Value> =
            list.as_array().unwrap().iter().filter(|c| c["Names"][0] != format!("/{name}").as_str()).collect();
        self.fixture.set_docker("/containers/json", &json!(kept).to_string());
    }
}

fn moi(host: &Arc<dyn Host>, args: &[&str]) -> (u8, String) {
    let mut out = Vec::new();
    match cli::execute(&cli::parse(args).unwrap(), host.clone(), &mut out) {
        Ok(()) => (0, String::from_utf8(out).unwrap()),
        Err(e) if e.exit_code() == 1 => (1, String::from_utf8(out).unwrap()),
        Err(e) => (e.exit_code(), e.to_string()),
    }
}

// ------------------------------------------------------------------ the socket session

#[test]
fn a_session_joins_answers_pings_and_keeps_what_the_server_emits() {
    let s = server("sync-session", "1.23.17");
    let mut session = Session::connect(&s.host, "http://uptime.home/").unwrap();
    // the server greets with `info`; an event that never comes is an error after a few polls, each
    // of which the fake answers with a ping that must be answered
    assert_eq!(session.event("info").unwrap()["serverTimezone"], "UTC");
    assert_eq!(session.emit("needSetup", &[]).unwrap(), json!(false));
    assert!(session.event_within("neverSent", 3).unwrap_err().contains("never sent `neverSent`"));
    let pings = s.kuma.state().pings_sent;
    let pongs = s.kuma.state().pongs;
    assert!(pings >= 3 && pongs == pings, "{pings} pings, {pongs} pongs");
    // acknowledgements are matched to their events
    assert_eq!(session.emit("getTags", &[]).unwrap()["msg"], "You are not logged in.");
    assert_eq!(session.emit("needSetup", &[]).unwrap(), json!(false));
    drop(session);
    // something that is not Kuma
    s.fixture.set_http(Arc::new(|_, _| b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n<html></html>".to_vec()));
    assert!(Session::connect(&s.host, "http://uptime.home").unwrap_err().starts_with("not a socket.io server"));
    s.fixture.set_http(Arc::new(|_, _| b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n".to_vec()));
    assert!(
        Kuma::connect(&s.host, "http://uptime.home")
            .unwrap_err()
            .starts_with("cannot reach Uptime Kuma at http://uptime.home: HTTP 502")
    );
}

#[test]
fn login_reads_the_version_and_the_inventory_matches_the_database() {
    let s = server("sync-login", "1.23.17");
    let mut kuma = Kuma::connect(&s.host, "http://uptime.home").unwrap();
    assert!(!kuma.needs_setup().unwrap());
    assert_eq!(kuma.login(USER, "wrong").unwrap_err(), "login failed: Incorrect username or password.");
    kuma.login(USER, PASSWORD).unwrap();
    assert_eq!(kuma.version, "1.23.17");
    // the live inventory says what the read-only one says (minus heartbeats and the file's path)
    let live = kuma.inventory().unwrap();
    let mut local = inventory::read(s.host.as_ref(), KUMA_DB).unwrap();
    local.db.clear();
    local.monitors.iter_mut().for_each(|m| m.last_status = None);
    assert_eq!(live.monitors.len(), 21);
    assert_eq!(live.monitors, local.monitors);
    assert_eq!((live.notifications, live.tags), (local.notifications, local.tags));
    // the password is not in what the fake logged, nor does the client ever put it in a URL
    assert!(!format!("{:?}", s.kuma.state().events).contains(PASSWORD));

    s.kuma.state().two_factor = true;
    let mut again = Kuma::connect(&s.host, "http://uptime.home").unwrap();
    assert!(again.login(USER, PASSWORD).unwrap_err().contains("two-factor authentication"));
}

// ------------------------------------------------------------------ sync

#[test]
fn sync_adopts_what_exists_creates_what_is_missing_and_then_has_nothing_to_do() {
    let s = server("sync-first", "1.23.17");
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    let before: Vec<Value> = s.kuma.state().monitors.values().cloned().collect();
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[1],
        "adopt   Server Resources            push    token …ok01 (fed by /opt/scripts/resource-check.sh — migration map)"
    );
    assert_eq!(lines[5], "create  cestmoi: snapshot           push    interval 93600s");
    assert_eq!(*lines.last().unwrap(), "4 adopted, 36 created");
    assert!(
        !out.contains("not managed by moi"),
        "monitors that are none of moi's business are not listed after a sync"
    );

    // nothing a human made was touched
    let state = s.kuma.state();
    for monitor in &before {
        assert_eq!(state.monitors.get(&monitor["id"].as_i64().unwrap()), Some(monitor), "{}", monitor["name"]);
    }
    assert_eq!(state.monitors.len(), 21 + 36);
    drop(state);

    // adopted: bound to the monitor that was there, with the token it always had
    let bindings = s.bindings();
    let resources = bindings.iter().find(|b| b.owner == "check:resources").unwrap();
    assert_eq!(
        (resources.kuma_id, resources.push_token.as_deref(), resources.created_by_moi),
        (Some(1), Some("FAKEresourcesTok01"), false)
    );
    assert_eq!(bindings.len(), 40);

    // created: a push monitor with its own token, the configured notification, moi's tag and a note
    let snapshot = s.kuma.monitor_named("cestmoi: snapshot").unwrap();
    let token = snapshot["pushToken"].as_str().unwrap();
    assert!(token.len() == 32 && token.bytes().all(|b| b.is_ascii_alphanumeric()), "{token}");
    assert_eq!(
        (snapshot["type"].as_str(), snapshot["interval"].as_u64(), snapshot["maxretries"].as_u64()),
        (Some("push"), Some(93_600), Some(1))
    );
    assert_eq!(snapshot["notificationIDList"], json!({ "1": true }));
    assert_eq!(snapshot["description"], "Managed by moi (freshness:snapshot)");
    assert_eq!(s.kuma.tags_of(snapshot["id"].as_i64().unwrap()), ["moi"]);
    let bound = bindings.iter().find(|b| b.owner == "freshness:snapshot").unwrap();
    assert_eq!(
        (bound.kuma_id, bound.push_token.as_deref(), bound.created_by_moi),
        (snapshot["id"].as_i64(), Some(token), true)
    );
    // a docker monitor on the docker host moi added, because Kuma's container mounts the socket
    let jellyfin = s.kuma.monitor_named("cestmoi: jellyfin").unwrap();
    let hosts = s.kuma.state().docker_hosts.clone();
    assert_eq!(hosts.len(), 1, "created once");
    assert_eq!(
        (hosts[0]["name"].as_str(), hosts[0]["dockerDaemon"].as_str(), hosts[0]["dockerType"].as_str()),
        (Some("local"), Some("/var/run/docker.sock"), Some("socket"))
    );
    assert_eq!(
        (
            jellyfin["type"].as_str(),
            jellyfin["docker_container"].as_str(),
            &jellyfin["docker_host"],
            jellyfin["interval"].as_u64()
        ),
        (Some("docker"), Some("jellyfin"), &hosts[0]["id"], Some(60))
    );
    assert!(jellyfin.get("conditions").is_none(), "a 1.x Kuma has no such column");
    assert_eq!(s.kuma.state().tags.iter().filter(|t| t["name"] == "moi").count(), 1, "the tag is created once");

    // the second sync changes nothing, in Kuma or in moi.db
    let changes = s.kuma.changes().len();
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.lines().last().unwrap(), "nothing to do: Uptime Kuma already has every monitor moi wants");
    assert_eq!(s.kuma.changes().len(), changes);
    assert_eq!(s.bindings(), bindings);
    let (_, json) = s.moi(&["monitors", "sync", "--json"]);
    let v: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        (v["changes"].as_u64(), v["kuma"].as_str(), v["result"]["created"].as_u64()),
        (Some(0), Some("1.23.17"), Some(0))
    );
    assert!(!json.contains(token), "tokens are never printed");

    // and now pushing works: to the adopted monitor with its old token, to a created one with its new token
    let (code, out) = s.moi(&["check", "resources", "--push"]);
    assert_eq!((code, out.as_str()), (0, "up: OK: RAM 38%, swap 2%, CPU 48C\n"));
    assert_eq!(s.kuma.state().pushes, ["FAKEresourcesTok01 up OK: RAM 38%, swap 2%, CPU 48C"]);
}

#[test]
fn a_2x_kuma_gets_the_fields_it_requires() {
    let s = server("sync-kuma2", "2.0.2");
    s.edit_config(|t| t.replace("containers = true", "containers = false"));
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.lines().last().unwrap(), "4 adopted, 1 created");
    let snapshot = s.kuma.monitor_named("cestmoi: snapshot").unwrap();
    assert_eq!((&snapshot["conditions"], &snapshot["accepted_statuscodes"]), (&json!([]), &json!(["200-299"])));
}

#[test]
fn created_monitors_go_into_the_configured_group_which_is_created_once() {
    let s = server("sync-group", "1.23.17");
    s.edit_config(|t| t.replace("containers = true", "containers = false\ngroup = \"cestmoi\""));
    assert_eq!(s.moi(&["monitors", "sync"]).0, 0);
    let group = s.kuma.monitor_named("cestmoi").unwrap();
    assert_eq!(group["type"], "group");
    assert_eq!(s.kuma.monitor_named("cestmoi: snapshot").unwrap()["parent"], group["id"]);
    // adopted monitors stay where their owner put them
    assert_eq!(s.kuma.monitor_named("Server Resources").unwrap()["parent"], Value::Null);
}

#[test]
fn a_container_that_is_gone_has_its_monitor_paused_and_only_prune_deletes_it() {
    let s = server("sync-orphan", "1.23.17");
    assert_eq!(s.moi(&["monitors", "sync"]).0, 0);
    let id = s.kuma.monitor_named("cestmoi: watchtower").unwrap()["id"].as_i64().unwrap();
    // a human's monitor that merely looks like one of moi's
    s.kuma.state().monitors.insert(900, json!({ "id": 900, "name": "cestmoi: by hand", "type": "docker", "active": true, "interval": 60, "docker_container": "gone" }));
    let moi_tag = s.kuma.state().tags.iter().find(|t| t["name"] == "moi").unwrap()["id"].as_i64().unwrap();
    s.kuma.state().monitor_tags.push((moi_tag, 900));

    s.remove_container("watchtower");
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("pause   cestmoi: watchtower") && out.ends_with("1 paused\n"), "{out}");
    assert_eq!(s.kuma.monitor_named("cestmoi: watchtower").unwrap()["active"], false);
    assert_eq!(s.kuma.changes().last().unwrap(), &format!("pauseMonitor {id}"));
    // paused once is enough
    assert!(
        s.moi(&["monitors", "sync"]).1.ends_with("nothing to do: Uptime Kuma already has every monitor moi wants\n")
    );

    let (code, out) = s.moi(&["monitors", "sync", "--prune"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.ends_with("1 deleted\n"), "{out}");
    assert!(s.kuma.monitor_named("cestmoi: watchtower").is_none());
    assert!(s.kuma.monitor_named("cestmoi: by hand").is_some(), "never a monitor moi did not create");
    assert_eq!(s.kuma.state().monitors.len(), 21 + 36 + 1 - 1);
    assert!(!s.bindings().iter().any(|b| b.owner == "container:watchtower"));
    let deletes: Vec<String> = s.kuma.changes().into_iter().filter(|e| e.starts_with("deleteMonitor")).collect();
    assert_eq!(deletes, [format!("deleteMonitor {id}")]);
}

#[test]
fn drift_is_reported_and_only_update_changes_the_monitor_keeping_everything_else() {
    let s = server("sync-update", "1.23.17");
    s.edit_config(|t| t.replace("containers = true", "containers = false"));
    {
        let mut state = s.kuma.state();
        let backup = state.monitors.get_mut(&4).unwrap();
        backup["interval"] = json!(86_400);
        backup["description"] = json!("nightly backup, set up by hand");
    }
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("drift   Server Backup             push  interval 86400 (moi default 90000)"), "{out}");
    assert_eq!(s.kuma.state().monitors[&4]["interval"], 86_400, "drift alone changes nothing");
    assert!(!s.kuma.changes().iter().any(|e| e.starts_with("editMonitor")));

    let (code, out) = s.moi(&["monitors", "sync", "--update"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("update  Server Backup") && out.ends_with("1 updated\n"), "{out}");
    let backup = s.kuma.state().monitors[&4].clone();
    assert_eq!(backup["interval"], 90_000);
    // never the token, the name, the notifications or anything else the owner set
    assert_eq!(
        (backup["pushToken"].as_str(), backup["name"].as_str(), backup["description"].as_str()),
        (Some("FAKEbackupTok00004"), Some("Server Backup"), Some("nightly backup, set up by hand"))
    );
    assert_eq!(backup["notificationIDList"], json!({ "1": true }));
    assert!(
        s.moi(&["monitors", "sync", "--update"])
            .1
            .ends_with("nothing to do: Uptime Kuma already has every monitor moi wants\n")
    );
}

#[test]
fn a_step_that_fails_is_reported_and_the_others_still_happen() {
    let s = server("sync-failure", "1.23.17");
    s.kuma.state().refuse_name = Some("cestmoi: jellyfin".to_string());
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!(code, 1);
    assert!(
        out.contains("failed: cestmoi: jellyfin: cannot create monitor \"cestmoi: jellyfin\": database is locked\n"),
        "{out}"
    );
    assert!(out.ends_with("4 adopted, 35 created, 1 failed\n"), "{out}");
    assert!(
        !s.bindings().iter().any(|b| b.owner == "container:jellyfin"),
        "nothing is bound to a monitor that does not exist"
    );
    // the next sync finishes the job
    s.kuma.state().refuse_name = None;
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!((code, out.lines().last().unwrap()), (0, "1 created"));
}

#[test]
fn container_monitors_need_a_docker_host_kuma_can_reach() {
    let s = server("sync-no-socket", "1.23.17");
    // Kuma's container without the Docker socket
    let list: Value = serde_json::from_slice(
        &std::fs::read(common::fixture_dir("cestmoi").join("docker/containers/json.json")).unwrap(),
    )
    .unwrap();
    let id = list.as_array().unwrap().iter().find(|c| c["Names"][0] == "/uptime-kuma").unwrap()["Id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut container: Value = serde_json::from_slice(
        &std::fs::read(common::fixture_dir("cestmoi").join(format!("docker/containers/{id}/json.json"))).unwrap(),
    )
    .unwrap();
    container["Mounts"].as_array_mut().unwrap().retain(|m| m["Destination"] != "/var/run/docker.sock");
    s.fixture.set_docker(&format!("/containers/{id}/json"), &container.to_string());
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!(code, 1);
    assert!(out.contains("failed: cestmoi: adguard: Uptime Kuma has no docker host \"local\" and its container does not mount /var/run/docker.sock"), "{out}");
    assert!(out.ends_with("4 adopted, 1 created, 35 failed\n"), "{out}");
    assert!(s.kuma.state().docker_hosts.is_empty());
    // a docker host of that name that already exists is used as it is
    s.kuma
        .state()
        .docker_hosts
        .push(json!({ "id": 7, "name": "local", "dockerDaemon": "tcp://10.0.0.1:2375", "dockerType": "tcp" }));
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!((code, out.lines().last().unwrap()), (0, "35 created"));
    assert_eq!(s.kuma.monitor_named("cestmoi: adguard").unwrap()["docker_host"], 7);
}

#[test]
fn sync_says_what_it_needs_when_it_cannot_log_in() {
    let s = server("sync-errors", "1.23.17");
    s.edit_config(|t| t.replace("username = \"secret:kuma_username\"\n", ""));
    let (code, err) = s.moi(&["monitors", "sync"]);
    assert_eq!(code, 2);
    assert!(
        err.starts_with("changing monitors needs a Uptime Kuma login: set username and password in [monitors.kuma]"),
        "{err}"
    );
    assert_eq!(s.moi(&["monitors", "sync", "--dry-run"]).0, 0, "the plan still works");

    let s = server("sync-errors-password", "1.23.17");
    s.kuma.state().users[0].1 = "changed".to_string();
    assert_eq!(s.moi(&["monitors", "sync"]), (2, "login failed: Incorrect username or password.".to_string()));
    assert_eq!(s.kuma.changes(), Vec::<String>::new());
    assert_eq!(s.bindings(), [], "nothing is bound when nothing could be checked");
}

#[test]
fn sharing_the_context_is_announced_on_the_freshness_monitor() {
    let s = server("sync-share-notify", "1.23.17");
    s.edit_config(|t| t.replace("containers = true", "containers = false"));
    assert_eq!(s.moi(&["monitors", "sync"]).0, 0);
    let token = s.kuma.monitor_named("cestmoi: snapshot").unwrap()["pushToken"].as_str().unwrap().to_string();
    // the fake Kuma is the whole network here: it does not know the bucket, so the upload fails after
    // nothing was announced — a share that did not happen is not announced
    let (code, _) = s.moi(&["context", "share"]);
    assert_ne!(code, 0);
    assert!(s.kuma.state().pushes.is_empty());
    // announce directly what the command announces
    let db = StateDir::new("/var/lib/moi").open_db(s.host.as_ref()).unwrap();
    let config = cestmoi::config::load(s.host.as_ref(), &cestmoi::config::Paths::new("/etc/moi/moi.toml".into(), None))
        .unwrap()
        .config;
    monitors::push(&s.host, &config, &db, "freshness:snapshot", "up", "context shared, expires 2026-11-26 18:00 UTC")
        .unwrap();
    assert_eq!(s.kuma.state().pushes, [format!("{token} up context shared, expires 2026-11-26 18:00 UTC")]);
    assert_eq!(
        monitors::push(&s.host, &config, &db, "boot:verify", "up", "x").unwrap_err(),
        "not pushed: no monitor is bound to boot:verify yet (run `moi monitors sync`)"
    );
}

// ------------------------------------------------------------------ against real Uptime Kuma servers (CI)

/// Forward the fixture's network to the real one: only the integration job does this, against the
/// service containers it started.
fn real_network() -> cestmoi::host::HttpHandler {
    Arc::new(|authority, raw| {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(authority).unwrap();
        stream.write_all(raw).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    })
}

/// The whole life of moi's monitors on a real Kuma: a fresh server gets its first user, a human makes
/// two monitors, then sync / sync / push / orphan / prune — checking after every step what Kuma itself
/// says it has.
fn against_a_real_kuma(test: &str, url: &str) {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    fixture.set_http(real_network());
    fixture.set_now(std::time::SystemTime::now());
    let host: Arc<dyn Host> = fixture.clone();
    let s = Server { host: host.clone(), fixture, kuma: FakeKuma::new("unused", "", "") };
    let (user, password) = ("moi-it", "FAKE-Kuma-passw0rd-for-CI!");
    s.edit_config(|t| {
        t.replace("url = \"http://uptime.home\"", &format!("url = \"{url}\""))
            .replace("notifications = [\"Email\"]", "notifications = []")
    });
    let secrets = Path::new("/etc/moi/secrets.toml");
    let text = host.read_to_string(secrets).unwrap().replace(USER, user).replace(PASSWORD, password);
    host.write(secrets, text.as_bytes(), 0o600).unwrap();

    let login = || {
        let mut kuma = Kuma::connect(&host, url).unwrap();
        kuma.login(user, password).unwrap();
        kuma
    };
    // a fresh Kuma has no user yet
    let mut first = Kuma::connect(&host, url).unwrap();
    if first.needs_setup().unwrap() {
        first.setup(user, password).unwrap();
    }
    drop(first);
    let mut human = login();
    eprintln!("Uptime Kuma {}", human.version);
    assert!(!human.version.is_empty(), "the version is known after login");
    // what a human set up before moi arrived: the storage monitor (same name moi wants) and one of their own
    let existing = human.inventory().unwrap();
    if !existing.monitors.iter().any(|m| m.name == "Server Storage") {
        human.add_monitor(json!({ "type": "push", "name": "Server Storage", "interval": 300, "pushToken": "FAKEhumanStorageToken00000000001" })).unwrap();
        human.add_monitor(json!({ "type": "push", "name": "Made by a human", "interval": 120, "pushToken": "FAKEhumanOwnMonitorToken00000002" })).unwrap();
    }
    drop(human);
    let human_made = |kuma: &mut Kuma| -> Vec<monitors::KumaMonitor> {
        kuma.inventory()
            .unwrap()
            .monitors
            .into_iter()
            .filter(|m| m.name == "Server Storage" || m.name == "Made by a human")
            .collect()
    };
    let before = human_made(&mut login());
    assert_eq!(before.len(), 2);
    assert_eq!(
        before[0].push_token.as_deref(),
        Some("FAKEhumanStorageToken00000000001"),
        "Kuma keeps the token it was given"
    );

    // ---- first sync: one adoption (by name), everything else created
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.lines().last().unwrap(), "1 adopted, 39 created", "{out}");
    let mut kuma = login();
    let live = kuma.inventory().unwrap();
    assert_eq!(human_made(&mut kuma), before, "monitors made by a human are untouched");
    let named = |name: &str| {
        live.monitors.iter().find(|m| m.name == name).unwrap_or_else(|| panic!("{name} missing in Kuma")).clone()
    };
    let resources = named("Server Resources");
    assert_eq!((resources.kind.as_str(), resources.interval, resources.active), ("push", Some(360), true));
    assert_eq!(resources.tags, ["moi"]);
    assert_eq!(resources.push_token.as_ref().map(String::len), Some(32));
    let jellyfin = named("cestmoi: jellyfin");
    assert_eq!(
        (jellyfin.kind.as_str(), jellyfin.docker_container.as_deref(), jellyfin.interval),
        ("docker", Some("jellyfin"), Some(60))
    );
    assert_eq!(jellyfin.tags, ["moi"]);
    let docker_host = live.docker_hosts.iter().find(|h| h.name == "local").expect("docker host created");
    assert_eq!((docker_host.daemon.as_str(), jellyfin.docker_host), ("/var/run/docker.sock", Some(docker_host.id)));
    assert_eq!(live.monitors.iter().filter(|m| m.tags.iter().any(|t| t == "moi")).count(), 39);
    assert_eq!(live.tags.iter().filter(|(_, name)| name == "moi").count(), 1);
    drop(kuma);
    let bindings = s.bindings();
    let storage = bindings.iter().find(|b| b.owner == "check:storage").unwrap();
    assert_eq!(
        (storage.push_token.as_deref(), storage.created_by_moi),
        (Some("FAKEhumanStorageToken00000000001"), false)
    );
    assert_eq!(bindings.iter().find(|b| b.owner == "check:resources").unwrap().push_token, resources.push_token);

    // ---- second sync: a no-op
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!(
        (code, out.lines().last().unwrap()),
        (0, "nothing to do: Uptime Kuma already has every monitor moi wants"),
        "{out}"
    );
    assert_eq!(login().inventory().unwrap().monitors.len(), live.monitors.len());

    // ---- pushes arrive: on the adopted monitor and on a created one
    for check in ["storage", "resources"] {
        let (_, out) = s.moi(&["check", check, "--push", "--json"]);
        let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
        assert_eq!(v["checks"][0]["pushed"], "ok", "{out}");
    }

    // ---- drift: the human's 300 s interval is reported, changed only with --update, and nothing else changes
    let (_, out) = s.moi(&["monitors", "sync"]);
    assert!(out.contains("interval 300 (moi default 360)"), "{out}");
    let (code, out) = s.moi(&["monitors", "sync", "--update"]);
    assert_eq!((code, out.lines().last().unwrap()), (0, "1 updated"), "{out}");
    let updated = human_made(&mut login());
    assert_eq!(
        (updated[0].interval, updated[0].push_token.as_deref(), updated[0].name.as_str()),
        (Some(360), Some("FAKEhumanStorageToken00000000001"), "Server Storage")
    );
    assert_eq!(updated[1], before[1]);

    // ---- a container goes away: its monitor is paused, not deleted
    s.remove_container("watchtower");
    let (code, out) = s.moi(&["monitors", "sync"]);
    assert_eq!((code, out.lines().last().unwrap()), (0, "1 paused"), "{out}");
    let live = login().inventory().unwrap();
    assert!(!live.monitors.iter().find(|m| m.name == "cestmoi: watchtower").unwrap().active);
    // ---- and deleted only with --prune; nothing else goes
    let (code, out) = s.moi(&["monitors", "sync", "--prune"]);
    assert_eq!((code, out.lines().last().unwrap()), (0, "1 deleted"), "{out}");
    let after = login().inventory().unwrap();
    assert!(!after.monitors.iter().any(|m| m.name == "cestmoi: watchtower"));
    assert_eq!(after.monitors.len(), live.monitors.len() - 1);
    assert_eq!(after.monitors.iter().filter(|m| m.name == "Made by a human").count(), 1);
}

#[test]
#[ignore = "needs MOI_IT services (Uptime Kuma 1.23.17 at MOI_IT_KUMA_URL)"]
fn sync_against_a_real_kuma_1_23() {
    against_a_real_kuma("sync-real-kuma1", &std::env::var("MOI_IT_KUMA_URL").expect("MOI_IT_KUMA_URL"));
}

#[test]
#[ignore = "needs MOI_IT services (Uptime Kuma 2.x at MOI_IT_KUMA2_URL)"]
fn sync_against_a_real_kuma_2() {
    against_a_real_kuma("sync-real-kuma2", &std::env::var("MOI_IT_KUMA2_URL").expect("MOI_IT_KUMA2_URL"));
}
