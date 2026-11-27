//! `moi init` and `moi uninstall` (docs/spec/01-config.md § Discovery).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use cestmoi::cli;
use cestmoi::config::{self, AutoOr, Config, Paths};
use cestmoi::host::{FixtureHost, Host};
use cestmoi::init;

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
}

/// The cestmoi machine before moi was ever set up on it: no `/etc/moi`, the binary just installed.
fn fresh(test: &str) -> Server {
    let case = common::scratch(&format!("{test}-case"));
    std::fs::write(case.join("absent"), "/etc/moi\n").unwrap();
    let overlay = common::scratch(test);
    let fixture = Arc::new(
        FixtureHost::open(&common::fixture_dir("cestmoi"))
            .unwrap()
            .with_case(&case)
            .unwrap()
            .with_overlay(&overlay)
            .unwrap(),
    );
    let s = Server { host: fixture.clone(), fixture };
    s.host.create_dir_all(Path::new("/usr/local/bin"), 0o755).unwrap();
    s.host.write(Path::new("/usr/local/bin/moi"), b"#!moi\n", 0o755).unwrap();
    s.fixture.add_command("systemctl daemon-reload", 0, "");
    s.fixture.add_command("systemctl enable --now moi.service", 0, "");
    s.fixture.add_command("systemctl disable --now moi.service", 0, "");
    s
}

impl Server {
    fn moi(&self, args: &[&str]) -> (u8, String) {
        let mut out = Vec::new();
        match cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out) {
            Ok(()) => (0, String::from_utf8(out).unwrap()),
            Err(e) => (e.exit_code(), e.to_string()),
        }
    }

    fn config(&self) -> Config {
        config::load(self.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap().config
    }

    fn systemctl(&self) -> Vec<String> {
        // the calls that change something (the snapshot asks systemd many questions)
        let changes = ["daemon-reload", "enable", "disable", "start", "stop", "restart"];
        let changing = |c: &String| {
            c.strip_prefix("systemctl ")
                .is_some_and(|rest| changes.contains(&rest.split(' ').next().unwrap_or_default()))
        };
        self.fixture.commands_run().into_iter().filter(changing).collect()
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

#[test]
fn discovery_finds_disks_mounts_data_and_what_runs() {
    let s = fresh("init-discover");
    let found = init::discover(&s.host);
    assert_eq!(found.server, "cestmoi");
    // an NVMe disk, and a SATA disk behind a USB bridge (which smartctl must be told about)
    assert_eq!(
        found.disks,
        [("/dev/nvme0n1".to_string(), "nvme".to_string()), ("/dev/sda".to_string(), "sat".to_string())]
    );
    // real filesystems that are not the system's: checked for space; the removable one must be writable
    assert_eq!(found.mounts, strings(&["/", "/mnt/storage"]));
    assert_eq!(found.writable, strings(&["/mnt/storage"]));
    assert_eq!(found.data_roots, strings(&["/home", "/mnt/storage", "/opt"]));
    assert!(found.docker);
    assert_eq!(found.kuma_url.as_deref(), Some("http://172.19.0.33:3001"));
    // media is proposed as replaceable, never for backup
    assert!(found.fungible.iter().any(|p| p.starts_with("/mnt/storage/media")), "{:?}", found.fungible);
    assert!(!found.backup_paths.iter().any(|p| p.contains("/media")), "{:?}", found.backup_paths);
    // stacks and the data containers keep outside them; never system directories or single files
    assert!(found.backup_paths.contains(&"/opt/yams".to_string()), "{:?}", found.backup_paths);
    assert!(
        found
            .backup_paths
            .iter()
            .all(|p| p.starts_with("/opt") || p.starts_with("/mnt/storage") || p.starts_with("/home")),
        "{:?}",
        found.backup_paths
    );
    assert!(
        !found.backup_paths.iter().any(|p| p.starts_with("/var/run") || p.starts_with("/etc") || p == "/"),
        "{:?}",
        found.backup_paths
    );
    // databases get the check their own client offers; apps an HTTP example on their first port
    assert!(
        found.exec_examples.contains(
            &r#"{ name = "outline-postgres", container = "outline-postgres", cmd = "pg_isready -q" }"#.to_string()
        ),
        "{:?}",
        found.exec_examples
    );
    assert!(
        found.exec_examples.contains(
            &r#"{ name = "outline-redis", container = "outline-redis", cmd = "redis-cli ping", expect = "PONG" }"#
                .to_string()
        )
    );
    assert!(
        found.http_examples.iter().any(|e| e == r#"{ name = "gluetun", target = "gluetun", path = "8080/" }"#),
        "{:?}",
        found.http_examples
    );
    assert!(!found.http_examples.iter().any(|e| e.contains("dokploy.1.")), "swarm tasks are their service's business");
    // the health scripts this server already has
    assert!(
        found.legacy.iter().any(|l| l == "a script that pushes to Uptime Kuma (/opt/scripts/resource-check.sh)"),
        "{:?}",
        found.legacy
    );
}

#[test]
fn init_writes_a_configuration_takes_a_snapshot_and_starts_the_service() {
    let s = fresh("init-fresh");
    let (code, out) = s.moi(&["init", "--non-interactive"]);
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "wrote /etc/moi/moi.toml and /etc/moi/secrets.toml");
    assert_eq!(lines[1], "found: 2 disks, 3 data roots, Docker, Uptime Kuma at http://172.19.0.33:3001");
    assert!(
        lines[2].starts_with("first snapshot: ") && lines[2].ends_with(" files in /var/lib/moi/state"),
        "{}",
        lines[2]
    );
    assert_eq!(lines[3], "moi.service installed and started (/usr/local/bin/moi daemon)");
    assert!(out.contains("  moi monitors sync --dry-run    what Uptime Kuma would be given\n"));
    assert!(
        out.ends_with("`moi migrate plan` shows how moi can take that over without breaking a monitor.\n"),
        "{out}"
    );

    // the configuration: exactly what discovery found, loadable, with nothing invented
    let text = s.host.read_to_string(Path::new("/etc/moi/moi.toml")).unwrap();
    common::assert_golden(
        &common::fixture_dir("cestmoi").parent().unwrap().parent().unwrap().join("expected/cestmoi/init.toml"),
        &text,
    );
    let config = s.config();
    assert!(
        config.checks.resources.common.enabled
            && config.checks.storage.common.enabled
            && config.checks.services.common.enabled
    );
    assert!(
        config.checks.services.http.is_empty() && config.checks.services.exec.is_empty(),
        "examples are comments, not checks"
    );
    assert!(config.monitors.kuma.enabled && config.monitors.kuma.username.is_none(), "no login is assumed");
    assert!(
        !config.firewall.manage && config.heal.is_empty() && !config.backup.enabled,
        "nothing that changes the machine is switched on"
    );
    assert!(text.contains("# [backup]\n# local_dir = \"/mnt/storage/backups\""), "backups are a proposal to uncomment");
    // secrets: the file exists, private, and empty of values
    let secrets = s.host.metadata(Path::new("/etc/moi/secrets.toml")).unwrap();
    assert_eq!(secrets.mode, 0o600);
    assert_eq!(s.host.metadata(Path::new("/etc/moi/moi.toml")).unwrap().mode, 0o644);

    // the one unit, enabled and started after the first snapshot exists
    let unit = s.host.read_to_string(Path::new("/etc/systemd/system/moi.service")).unwrap();
    assert!(
        unit.contains(
            "Type=notify\nExecStart=/usr/local/bin/moi daemon\nRestart=always\nRestartSec=5\nWatchdogSec=60\n"
        ),
        "{unit}"
    );
    assert_eq!(s.systemctl(), ["systemctl daemon-reload", "systemctl enable --now moi.service"]);
    assert!(
        s.host.exists(Path::new("/var/lib/moi/state/snapshot/meta/hostname"))
            && s.host.exists(Path::new("/var/lib/moi/state/CONTEXT.md"))
    );
    // the daemon it starts would run exactly these
    let loaded = config::load(s.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let daemon = cestmoi::daemon::Daemon::new(s.host.clone(), loaded).unwrap();
    let jobs: Vec<&str> = daemon.jobs().iter().map(|j| j.name.as_str()).collect();
    assert_eq!(jobs, ["check:resources", "check:storage", "check:services", "daily", "snapshot", "boot"]);
}

#[test]
fn what_init_writes_agrees_with_the_hand_written_cestmoi_configuration() {
    let s = fresh("init-equivalent");
    assert_eq!(s.moi(&["init"]).0, 0);
    let generated = s.config();
    // the real deployment's configuration, on the same machine
    let overlay = common::scratch("init-equivalent-example");
    let host: Arc<dyn Host> =
        Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    let example = config::load(host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap().config;

    // discovered facts are the same facts
    assert_eq!(generated.server.name, example.server.name);
    assert_eq!(generated.checks.storage.mounts, example.checks.storage.mounts);
    assert_eq!(generated.checks.storage.writable, example.checks.storage.writable);
    let disks = |c: &Config| match &c.checks.storage.disks {
        AutoOr::List(disks) => disks.iter().map(|d| (d.device.clone(), d.kind.clone())).collect::<Vec<_>>(),
        AutoOr::Auto => Vec::new(),
    };
    assert_eq!(disks(&generated), disks(&example));
    let roots = |c: &Config| match &c.capture.data_roots {
        AutoOr::List(roots) => {
            let mut roots = roots.clone();
            roots.sort();
            roots
        }
        AutoOr::Auto => Vec::new(),
    };
    assert_eq!(roots(&generated), roots(&example));
    // everything init proposes to protect or to declare replaceable, the owner dealt with one way or the other
    let dealt_with: Vec<&String> = example.backup.paths.iter().chain(&example.data.fungible).collect();
    let covered = |path: &String| {
        dealt_with
            .iter()
            .any(|have| path == *have || path.starts_with(&format!("{have}/")) || have.starts_with(&format!("{path}/")))
    };
    let proposed = init::discover(&s.host);
    assert!(!proposed.backup_paths.is_empty());
    for path in proposed.backup_paths.iter().chain(&generated.data.fungible) {
        assert!(
            covered(path),
            "{path} is proposed by init and neither backed up nor declared fungible in examples/cestmoi.toml"
        );
    }
    // and init says so itself when run on the configured machine
    let (code, out) = {
        let mut out = Vec::new();
        let code = cli::execute(&cli::parse(&["init"]).unwrap(), host.clone(), &mut out).map_or(1, |()| 0);
        (code, String::from_utf8(out).unwrap())
    };
    assert_eq!(
        (code, out.as_str()),
        (
            0,
            "/etc/moi/moi.toml exists and was not changed.\nIt covers everything `moi init` would propose for this server.\n"
        )
    );
}

#[test]
fn an_existing_configuration_is_never_overwritten_only_compared() {
    let s = fresh("init-rerun");
    s.host.create_dir_all(Path::new("/etc/moi"), 0o755).unwrap();
    let mine =
        "# mine\n[checks.resources]\n\n[checks.storage]\ndisks = [{ device = \"/dev/nvme0n1\", type = \"nvme\" }]\n";
    s.host.write(Path::new("/etc/moi/moi.toml"), mine.as_bytes(), 0o644).unwrap();
    let (code, out) = s.moi(&["init"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(s.host.read_to_string(Path::new("/etc/moi/moi.toml")).unwrap(), mine);
    assert!(
        out.starts_with(
            "/etc/moi/moi.toml exists and was not changed.\nWhat `moi init` would propose that it does not have:\n"
        ),
        "{out}"
    );
    for line in [
        "  checks.storage.disks: { device = \"/dev/sda\", type = \"sat\" } is not checked",
        "  checks.storage.mounts: \"/mnt/storage\" is not checked",
        "  [checks.services] is not enabled (containers are running here)",
        "  [monitors.kuma] is not set up (Uptime Kuma runs here: url = \"http://172.19.0.33:3001\")",
        "  backup.paths: \"/opt/yams\" is neither backed up nor declared fungible",
    ] {
        assert!(out.contains(&format!("{line}\n")), "missing {line:?} in:\n{out}");
    }
    assert!(!s.host.exists(Path::new("/etc/systemd/system/moi.service")), "nothing is installed on a re-run");
    assert_eq!(s.systemctl(), Vec::<String>::new());
}

#[test]
fn observe_only_describes_the_server() {
    let s = fresh("init-observe");
    s.host.remove_file(Path::new("/usr/local/bin/moi")).unwrap();
    let (code, out) = s.moi(&["init", "--observe"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("no service installed (--observe)"), "{out}");
    let config = s.config();
    assert!(
        !config.checks.resources.common.enabled
            && !config.checks.storage.common.enabled
            && !config.checks.services.common.enabled
    );
    assert!(!config.monitors.kuma.enabled && config.backup.paths.is_empty());
    assert!(!s.host.exists(Path::new("/etc/systemd/system/moi.service")));
    assert_eq!(s.systemctl(), Vec::<String>::new());
    assert!(s.host.exists(Path::new("/var/lib/moi/state/CONTEXT.md")), "but the server is described");
}

#[test]
fn nothing_is_written_when_the_service_could_not_be_installed() {
    let s = fresh("init-no-binary");
    s.host.remove_file(Path::new("/usr/local/bin/moi")).unwrap();
    let (code, err) = s.moi(&["init"]);
    assert_eq!(code, 2);
    assert!(err.starts_with("moi is not installed where a service can start it"), "{err}");
    assert!(!s.host.exists(Path::new("/etc/moi/moi.toml")) && !s.host.exists(Path::new("/var/lib/moi/state/snapshot")));
}

#[test]
fn a_server_without_docker_gets_a_configuration_without_container_sections() {
    let overlay = common::scratch("init-minimal");
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("minimal")).unwrap().with_overlay(&overlay).unwrap());
    let host: Arc<dyn Host> = fixture.clone();
    let text = init::config_text(&init::discover(&host), false);
    assert!(text.contains("[checks.resources]\n") && text.contains("[checks.storage]\nmounts = [\"/\"]\n"), "{text}");
    for absent in ["[checks.services]", "[monitors.kuma]", "[backup]", "[data]"] {
        assert!(!text.contains(absent), "{absent} in:\n{text}");
    }
    // and it loads
    common::write_config(&fixture, &text, None);
    config::load(host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
}

#[test]
fn uninstall_removes_what_moi_installed_and_keeps_what_is_the_owners() {
    let overlay = common::scratch("init-uninstall");
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    let s = Server { host: fixture.clone(), fixture };
    s.fixture.add_command("systemctl daemon-reload", 0, "");
    s.fixture.add_command("systemctl disable --now moi.service", 0, "");
    // moi as it is after a while: unit, state, the context copy and the pointer it maintains
    s.host.write(Path::new(init::UNIT_PATH), init::unit_text("/usr/bin/moi").as_bytes(), 0o644).unwrap();
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    assert!(s.host.exists(Path::new("/home/prabhas/SERVER_CONTEXT.md")));
    let pointer = s.host.read_to_string(Path::new("/home/prabhas/.claude/CLAUDE.md")).unwrap();
    assert!(pointer.contains("moi:begin"));

    let (code, out) = s.moi(&["uninstall"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.starts_with(
            "stopped and removed /etc/systemd/system/moi.service\nremoved /home/prabhas/SERVER_CONTEXT.md\n"
        ),
        "{out}"
    );
    assert!(out.contains("kept: /etc/moi (your configuration and secrets)\nkept: /var/lib/moi (history and archive; `moi uninstall --purge` deletes them)\n"), "{out}");
    assert!(out.ends_with(
        "Monitors in Uptime Kuma, firewall rules in the kernel and backups in their repository are untouched.\n"
    ));
    assert_eq!(s.systemctl(), ["systemctl disable --now moi.service", "systemctl daemon-reload"]);
    assert!(!s.host.exists(Path::new(init::UNIT_PATH)) && !s.host.exists(Path::new("/home/prabhas/SERVER_CONTEXT.md")));
    assert!(
        !s.host
            .read_to_string(Path::new("/home/prabhas/.claude/CLAUDE.md"))
            .is_ok_and(|text| text.contains("moi:begin")),
        "the pointer block is gone"
    );
    assert!(s.host.exists(Path::new("/var/lib/moi/moi.db")) && s.host.exists(Path::new("/etc/moi/moi.toml")));
    assert!(s.fixture.docker_requests().iter().all(|r| r.starts_with("GET ")), "nothing was touched in Docker");
    // a second time there is nothing left to remove
    assert!(s.moi(&["uninstall"]).1.starts_with("nothing of moi was installed\n"));

    // --purge deletes history for good: only for someone who names the server
    for answer in [None, Some("yes"), Some("CESTMOI")] {
        if let Some(answer) = answer {
            s.fixture.answer(answer);
        }
        assert_eq!(s.moi(&["uninstall", "--purge"]), (2, "not confirmed: nothing was removed".to_string()));
        assert!(s.host.exists(Path::new("/var/lib/moi/moi.db")));
    }
    assert!(s.fixture.asked()[0].ends_with("Type the server's name (cestmoi) to go on: "));
    s.fixture.answer("cestmoi");
    let (code, out) = s.moi(&["uninstall", "--purge"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("deleted /var/lib/moi\n") && !out.contains("kept: /var/lib/moi"), "{out}");
    assert!(!s.host.exists(Path::new("/var/lib/moi")));
    assert!(s.host.exists(Path::new("/etc/moi/moi.toml")), "the configuration is the owner's, purge or not");
}
