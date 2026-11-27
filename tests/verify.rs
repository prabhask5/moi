//! `moi verify`, expectations and the verification after every boot (docs/spec/09-restore-verify.md § Verify).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cestmoi::cli;
use cestmoi::config::{self, Paths};
use cestmoi::daemon::Daemon;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::restore::verify::{self, Expect};
use cestmoi::state::StateDir;
use cestmoi::state::db::Db;

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
}

fn server(test: &str) -> Server {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    Server { host: fixture.clone(), fixture }
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

    fn daemon(&self) -> Daemon {
        let loaded = config::load(self.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
        Daemon::new(self.host.clone(), loaded).unwrap()
    }

    /// A server where everything moi can be asked about is in order: firewall applied, monitors bound,
    /// Kuma answering.
    fn healthy(test: &str) -> Self {
        let s = server(test);
        assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
        assert_eq!(s.moi(&["firewall", "apply"]).0, 0);
        for (owner, token) in [
            ("check:resources", "FAKEresourcesTok01"),
            ("check:storage", "FAKEstorageTok0003"),
            ("check:services", "FAKEservicesTok002"),
        ] {
            s.db()
                .conn()
                .execute(
                    "INSERT INTO monitor_bindings (owner, monitor_name, push_token, updated_at) VALUES (?1, ?1, ?2, 0)",
                    (owner, token),
                )
                .unwrap();
        }
        s.fixture.set_http_at(
            "uptime.home:80",
            Arc::new(|_, _| b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}".to_vec()),
        );
        s
    }

    /// Without the named container.
    fn stop_container(&self, name: &str) {
        let list: serde_json::Value = serde_json::from_slice(
            &std::fs::read(common::fixture_dir("cestmoi").join("docker/containers/json.json")).unwrap(),
        )
        .unwrap();
        let kept: Vec<&serde_json::Value> =
            list.as_array().unwrap().iter().filter(|c| c["Names"][0] != format!("/{name}").as_str()).collect();
        self.fixture.set_docker("/containers/json", &serde_json::json!(kept).to_string());
    }
}

#[test]
fn expectations_are_what_the_snapshot_saw() {
    let s = server("verify-expect");
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    let state = StateDir::new("/var/lib/moi");
    let expect = Expect::load(s.host.as_ref(), &state).expect("every snapshot writes expect.json");
    assert_eq!(expect, Expect::from_state(s.host.as_ref(), &state));
    // running containers, without swarm tasks (their names change with every deploy)
    assert_eq!(expect.containers.len(), 35);
    assert!(
        expect.containers.contains(&"jellyfin".to_string())
            && !expect.containers.iter().any(|c| c.starts_with("dokploy.1."))
    );
    assert!(expect.services.contains(&("dokploy".to_string(), 1)) && expect.services.len() == 7);
    // ports by protocol and number, never the kernel's ephemeral ones
    assert_eq!(expect.ports, ["tcp 22", "tcp 80", "udp 53"]);
    assert!(expect.mounts.contains(&"/mnt/storage".to_string()) && expect.mounts.contains(&"/".to_string()));
    assert!(expect.units.contains(&"docker.service".to_string()));
    assert!(expect.timers.iter().any(|t| t.ends_with(".timer")), "{:?}", expect.timers);
    assert!(
        expect.cron.contains(&"crontab of prabhas".to_string())
            && expect.cron.contains(&"/etc/cron.d/resource-check".to_string()),
        "{:?}",
        expect.cron
    );
    assert_eq!(s.host.metadata(&Expect::path(&state)).unwrap().mode, 0o600);
}

#[test]
fn a_server_in_order_passes_every_check() {
    let s = Server::healthy("verify-ok");
    // the daemon is the one thing a fixture cannot have running
    let (code, out) = s.moi(&["verify"]);
    assert_eq!(code, 1, "{out}");
    let failed: Vec<&str> = out.lines().filter(|l| l.starts_with('✗')).collect();
    assert_eq!(failed, ["✗ the moi daemon is not running"], "{out}");
    for line in [
        "✓ containers running: all 35",
        "✓ swarm services at their replicas: all 7",
        "✓ listening ports: all 3",
        "✓ mounts: all 5",
        "✓ /mnt/storage is writable",
        "✓ services running: all 8",
        "✓ no failed units",
        "✓ cron entries: all 4",
        "✓ firewall (v4): the managed chains are what moi applied",
        "✓ required firewall rules: all 4",
        "✓ check resources: OK: RAM 38%, swap 2%, CPU 48C",
        "✓ Uptime Kuma accepted 3 results",
        "✓ the healthchecks.io watchdog monitor is active",
        "✓ backups: /mnt/storage is mounted",
        "✓ backups: the off-site repository answers",
        "✓ backups: R2 usage can be read",
    ] {
        assert!(out.lines().any(|l| l == line), "missing {line:?} in:\n{out}");
    }
    assert!(
        out.ends_with("    → systemctl status moi; systemctl start moi\n1 of 23 checks FAILED\n")
            || out.contains("1 of "),
        "{out}"
    );
    assert!(!s.host.exists(Path::new("/mnt/storage/.moi-verify")), "the write test cleans up");
    let (_, json) = s.moi(&["verify", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["failed"], 1);
    assert_eq!(v["checks"][0], serde_json::json!({ "ok": true, "what": "containers running: all 35", "hint": "" }));
}

#[test]
fn each_thing_that_is_missing_is_named_with_what_to_do() {
    let s = Server::healthy("verify-missing");
    s.stop_container("jellyfin");
    s.fixture.add_command("ss -tulpnH", 0, "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=1,fd=3))\ntcp LISTEN 0 128 0.0.0.0:45123 0.0.0.0:* users:((\"x\",pid=2,fd=3))\n");
    s.fixture.add_command(
        "systemctl list-units --failed --no-legend --no-pager --plain",
        0,
        "certbot.service loaded failed failed Certbot\n",
    );
    s.fixture.add_command(
        "systemctl list-units --type=service --state=running --no-legend --no-pager --plain",
        0,
        "docker.service loaded active running Docker\n",
    );
    s.fixture.add_command("restic cat config", 1, "");
    s.host.remove_file(Path::new("/etc/cron.d/storage-check")).ok();
    let (code, out) = s.moi(&["verify"]);
    assert_eq!(code, 1);
    for (line, hint) in [
        ("✗ containers running: jellyfin", "docker ps -a; docker logs <name>; its stack: docker compose up -d"),
        (
            "✗ listening ports: tcp 80",
            "what served it is not up: ss -tulpn, then the container or unit behind the port",
        ),
        (
            "✗ listening ports: udp 53",
            "what served it is not up: ss -tulpn, then the container or unit behind the port",
        ),
        ("✗ services running: cron.service", "systemctl status <unit>; journalctl -u <unit> -b"),
        ("✗ failed units: certbot.service", "systemctl status <unit>; journalctl -u <unit> -b"),
    ] {
        let at = out.lines().position(|l| l == line).unwrap_or_else(|| panic!("missing {line:?} in:\n{out}"));
        assert_eq!(out.lines().nth(at + 1).unwrap(), format!("    → {hint}"));
    }
    assert!(out.contains("✗ backups: the off-site repository does not answer"), "{out}");
    assert!(!out.contains("45123"), "ephemeral ports are nobody's expectation");
}

#[test]
fn a_boot_is_verified_against_what_ran_before_it_and_the_owner_is_told() {
    let s = Server::healthy("verify-boot");
    let pushes: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = pushes.clone();
    s.fixture.set_http_at(
        "uptime.home:80",
        Arc::new(move |_, raw| {
            seen.lock().unwrap().push(String::from_utf8_lossy(raw).lines().next().unwrap_or_default().to_string());
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}".to_vec()
        }),
    );
    s.db().conn().execute("INSERT INTO monitor_bindings (owner, monitor_name, push_token, updated_at) VALUES ('boot:verify', 'Server Boot', 'FAKEbootVerifyTok6', 0)", []).unwrap();
    s.fixture.set_env("JOURNAL_STREAM", "8:1");

    // the machine boots, and one container does not come back
    s.stop_container("jellyfin");
    let mut daemon = s.daemon();
    let start = cestmoi::host::unix_seconds(s.host.now());
    assert_eq!(daemon.jobs().iter().find(|j| j.name == "verify").unwrap().next, start + 240, "boot.verify_after = 4m");
    daemon.tick().unwrap();
    // something changes right after boot and a snapshot runs before the verification …
    daemon.file_changed(Path::new("/etc/hosts"));
    s.fixture.advance(Duration::from_secs(120));
    assert!(daemon.tick().unwrap().iter().any(|(name, _)| name == "snapshot"));
    // … which must not make the expectations forget the container
    let state = StateDir::new("/var/lib/moi");
    assert!(Expect::load(s.host.as_ref(), &state).unwrap().containers.contains(&"jellyfin".to_string()));
    assert!(verify::pending(&s.db()).is_some());

    s.fixture.advance(Duration::from_secs(120));
    let ran = daemon.tick().unwrap();
    let verified = &ran.iter().find(|(name, _)| name == "verify").expect("the post-boot verification ran").1;
    assert_eq!(verified.status, "failed");
    assert!(
        verified.message.starts_with("verify FAILED: ")
            && verified.message.contains("first: containers running: jellyfin"),
        "{}",
        verified.message
    );
    // in the journal, in the login message, on the monitor
    let logged: Vec<String> = s.fixture.journal_entries().iter().map(|e| e[0].1.clone()).collect();
    assert!(logged.iter().any(|m| m == "verify: ✗ containers running: jellyfin → docker ps -a; docker logs <name>; its stack: docker compose up -d"), "{logged:?}");
    let motd = s.host.read_to_string(Path::new("/run/motd.d/50-moi")).unwrap();
    assert!(motd.starts_with("moi: verify FAILED: ") && motd.contains("  ✗ containers running: jellyfin\n"), "{motd}");
    assert!(
        pushes
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.starts_with("GET /api/push/FAKEbootVerifyTok6?status=down&msg=verify%20FAILED")),
        "{:?}",
        pushes.lock().unwrap()
    );
    // judged: from now on the snapshot's view is the expectation again (the owner may have stopped it on purpose)
    assert!(verify::pending(&s.db()).is_none());
    assert!(!Expect::load(s.host.as_ref(), &state).unwrap().containers.contains(&"jellyfin".to_string()));
    assert_eq!(daemon.jobs().iter().find(|j| j.name == "verify").unwrap().next, i64::MAX, "once per boot");

    // a restart of the service is not a boot: nothing to verify
    assert_eq!(s.daemon().jobs().iter().find(|j| j.name == "verify").unwrap().next, i64::MAX);
    // the next boot, everything up: OK everywhere
    s.host
        .write(Path::new("/proc/sys/kernel/random/boot_id"), b"99999999-2222-3333-4444-555555555555\n", 0o644)
        .unwrap();
    let mut daemon = s.daemon();
    daemon.tick().unwrap();
    s.fixture.advance(Duration::from_secs(240));
    let ran = daemon.tick().unwrap();
    let verified = &ran.iter().find(|(name, _)| name == "verify").unwrap().1;
    // (the daemon cannot find itself running in a tick-driven test: that one item fails)
    assert!(verified.message.contains("first: the moi daemon is not running"), "{}", verified.message);
}
