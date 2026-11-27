//! Audit (docs/spec/04-audit.md) on the `cestmoi` fixture: no false positives for what is fine, true
//! positives for what is not.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cestmoi::audit::{self, Audit, Level};
use cestmoi::cli;
use cestmoi::config::{self, Paths};
use cestmoi::docker::Docker;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::snapshot;
use cestmoi::state::StateDir;

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
    state: StateDir,
    overlay: PathBuf,
}

fn server(test: &str) -> Server {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    Server { host: fixture.clone(), fixture, state: StateDir::new("/var/lib/moi"), overlay }
}

impl Server {
    /// Rewrite the fixture's moi.toml for this test.
    fn config(&self, edit: impl FnOnce(String) -> String) {
        let path = Path::new("/etc/moi/moi.toml");
        let text = edit(self.host.read_to_string(path).unwrap());
        self.host.write(path, text.as_bytes(), 0o644).unwrap();
    }

    /// Snapshot, then audit.
    fn audit(&self) -> Audit {
        let loaded = config::load(self.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
        let config = Arc::new(loaded.config);
        self.state.ensure(self.host.as_ref()).unwrap();
        let db = self.state.open_db(self.host.as_ref()).unwrap();
        snapshot::capture(&self.host, &config, &self.state, &db, &loaded.masker).unwrap();
        audit::run(&self.host, &config, &self.state).unwrap()
    }

    fn change_container(&self, name: &str, change: impl FnOnce(&mut serde_json::Value)) {
        let mut c = Docker::new(self.host.clone()).container(name).unwrap();
        change(&mut c);
        self.fixture.set_docker(&format!("/containers/{}/json", c["Id"].as_str().unwrap()), &c.to_string());
    }

    /// Replace the canned output of a command the fixture knows.
    fn recan(&self, line: &str, edit: impl FnOnce(String) -> String) {
        let key = FixtureHost::command_key(line);
        let file = common::fixture_dir("cestmoi").join("commands").join(format!("{key}.out"));
        let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{line}: {e}"));
        self.fixture.add_command(line, 0, &edit(text));
    }
}

fn texts(audit: &Audit, section: &str, level: Level) -> Vec<String> {
    audit.findings(level).into_iter().filter(|(id, _)| *id == section).map(|(_, t)| t.to_string()).collect()
}

fn has(audit: &Audit, section: &str, level: Level, needle: &str) -> bool {
    texts(audit, section, level).iter().any(|t| t.contains(needle))
}

const ENABLED: &str = "systemctl list-unit-files --state=enabled --no-legend --no-pager";

// ------------------------------------------------------------------ the fixture as it is

#[test]
fn golden_audit_of_the_cestmoi_fixture() {
    let s = server("audit-golden");
    let audit = s.audit();
    let expected = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/cestmoi/audit.md");
    common::assert_golden(&expected, &audit.markdown(1));
    assert_eq!(s.host.read_to_string(&s.state.audit()).unwrap(), audit.markdown(1));
    assert_eq!(s.host.metadata(&s.state.audit()).unwrap().mode, 0o600);
    // inside CONTEXT.md the same text sits one heading level lower
    assert!(audit.markdown(2).starts_with("## Coverage & reboot audit — cestmoi\n"));
    assert!(audit.markdown(2).contains("\n### Would it survive a reboot?\n"));
}

#[test]
fn nothing_that_is_fine_is_reported_as_a_gap() {
    let s = server("audit-no-false-positives");
    let audit = s.audit();
    // reboot: socket-activated ssh, the Incus loop mount, the firewall, docker
    assert_eq!(texts(&audit, "reboot", Level::Gap), Vec::<String>::new());
    assert_eq!(texts(&audit, "reboot", Level::Review), Vec::<String>::new());
    assert!(has(
        &audit,
        "reboot",
        Level::Ok,
        "service **ssh.service** is disabled but started on demand by ssh.socket"
    ));
    assert!(has(
        &audit,
        "reboot",
        Level::Ok,
        "`/var/lib/incus/storage-pools/default` (a loop image) is mounted at boot by incus-startup.service"
    ));
    assert!(has(&audit, "reboot", Level::Ok, "docker.service is enabled"));
    // docker data: read-only views, log binds and host files are nobody's data to lose; fungible paths are declared
    let docker_gaps = texts(&audit, "docker", Level::Gap);
    assert_eq!(docker_gaps.len(), 1, "{docker_gaps:#?}");
    for fine in
        ["/var/log/server-backup.log", "/var/run/docker.sock", "/mnt/storage/media", "/var/lib/incus", "/run/udev"]
    {
        assert!(!docker_gaps.iter().any(|g| g.contains(fine)), "{fine}");
    }
    assert_eq!(
        texts(&audit, "docker", Level::Fungible),
        [
            "`/mnt/storage/media`: media library — temporary, re-downloadable",
            "`/var/lib/incus`: devbox VM — rebuilt from github.com/prabhask5/dotfiles",
        ],
        "each fungible path is shown once, however many containers mount it"
    );
    // every volume and bind mount of the Dokploy services is in the backup (volumes = \"auto\")
    assert!(!docker_gaps.iter().any(|g| g.contains("stellar-data") || g.contains("dokploy-docker-config")));
    // AdGuard listens on the host, but its container brings it back
    assert_eq!(
        texts(&audit, "listeners", Level::Ok),
        ["udp 0.0.0.0:53 — AdGuardHome: provided by container adguard (host network)"]
    );
    assert_eq!(texts(&audit, "listeners", Level::Review), Vec::<String>::new());
}

#[test]
fn real_problems_are_found() {
    let s = server("audit-true-positives");
    let audit = s.audit();
    assert!(has(
        &audit,
        "docker",
        Level::Gap,
        "container **supabase-pooler** has 350 MB written inside the container itself"
    ));
    assert!(has(
        &audit,
        "config",
        Level::Ok,
        "`/etc/systemd/resolved.conf` differs from the version its package ships"
    ));
    assert!(has(&audit, "config", Level::Gap, "1 custom file(s) in /etc that no package owns"));
    assert!(audit.markdown(1).contains("```\n/etc/sysctl.d/99-homelab.conf\n```"));
    assert!(has(
        &audit,
        "monitoring",
        Level::Review,
        "`/opt/scripts/backup.sh.bak` is a stale copy holding a token no monitor has (…pTok)"
    ));
    assert!(has(
        &audit,
        "monitoring",
        Level::Review,
        "monitor **Server Backup** last received a malformed message (`[object Object]`)"
    ));
    assert!(has(&audit, "secrets", Level::Review, "`/opt/scripts/service-check.sh`: SONARR_KEY"));
    assert!(has(&audit, "state", Level::Gap, "`/var/lib/private-app`"));
    assert!(has(
        &audit,
        "docker",
        Level::Review,
        "image **stellar-planner-x7k2:latest** has no registry digest: built by Dokploy from git"
    ));
    assert!(
        has(&audit, "docker", Level::Review, "`/opt/services/outline/pgdata`")
            && has(&audit, "docker", Level::Review, "covered by the dump of outline-postgres")
    );
    // tokens are never printed in full
    assert!(!audit.markdown(1).contains("FAKEoldBackupTok") && !audit.markdown(1).contains("FAKEsonarr"));
    assert_eq!(audit.gaps(), 5);
}

#[test]
fn storage_placement_follows_the_disk_audit_script() {
    let s = server("audit-placement");
    let audit = s.audit();
    assert!(has(&audit, "placement", Level::Info, "Docker data root `/var/lib/docker` is on SSD"));
    // HOT keyword (database) on the USB hard disk
    assert_eq!(
        texts(&audit, "placement", Level::Gap),
        ["**supabase-db** `/etc/postgresql-custom` ← `/mnt/storage/database/db-config`: database/config on HDD"]
    );
    let reviews = texts(&audit, "placement", Level::Review);
    // BULK keywords on the NVMe: `/data$` matches a mount point named /data or ending in /data
    assert!(
        reviews.contains(&"**portainer** `/data` ← `/opt/yams/config/portainer`: bulk data on SSD".to_string()),
        "{reviews:#?}"
    );
    assert!(reviews.contains(
        &"**uptime-kuma** `/app/data` ← `/opt/services/monitoring/uptime-kuma/data`: bulk data on SSD".to_string()
    ));
    // two bulk mounts in one container: hardlinks cannot cross them
    assert!(
        reviews.contains(
            &"**qbittorrent** has 2 separate bulk mounts: hardlinks between them cannot work — use one `/data` mount"
                .to_string()
        )
    );
    // bulk on the big disk is where it belongs; single files are skipped; the keyword order is the script's (bulk first)
    let report = audit.markdown(1);
    assert!(report.contains("| jellyfin | HDD | bind | /data | /mnt/storage/media |  |"));
    assert!(
        report.contains("| supabase-db | HDD | bind | /var/lib/postgresql/data | /mnt/storage/database/db-data |  |")
    );
    assert!(
        !report.contains("envoy.yaml") && !report.contains("| host-logs |"),
        "single-file mounts are configuration"
    );
    assert!(report.contains("| stellar-planner-x7k2 | SSD | volume | /app/data | /var/lib/docker/volumes/stellar-data/_data | bulk data on SSD |"));
}

#[test]
fn custom_placement_keywords_replace_the_defaults() {
    let s = server("audit-placement-custom");
    s.config(|t| format!("{t}\n[audit.placement]\nhot = [\"/gluetun$\"]\nbulk = [\"media\"]\n"));
    let audit = s.audit();
    assert_eq!(texts(&audit, "placement", Level::Gap), Vec::<String>::new());
    let reviews = texts(&audit, "placement", Level::Review);
    assert!(reviews.iter().all(|r| !r.contains("portainer") && !r.contains("uptime-kuma")), "{reviews:#?}");
}

// ------------------------------------------------------------------ reboot survival

#[test]
fn the_firewall_is_safe_when_a_unit_reapplies_it_and_a_gap_when_nothing_does() {
    let s = server("audit-firewall");
    s.config(|t| t.replace("[firewall]\nmanage = true", "[firewall]\nmanage = false"));
    let audit = s.audit();
    assert!(
        has(&audit, "reboot", Level::Ok, "6 custom iptables rules are re-applied at boot by host-firewall.service"),
        "{:#?}",
        texts(&audit, "reboot", Level::Ok)
    );

    // the unit is no longer enabled: the rules only live in the kernel
    s.recan(ENABLED, |out| {
        out.lines().filter(|l| !l.starts_with("host-firewall.service")).map(|l| format!("{l}\n")).collect()
    });
    let audit = s.audit();
    assert!(has(&audit, "reboot", Level::Gap, "6 custom iptables rules are live but nothing saves or re-applies them"));
    assert!(audit.markdown(1).contains("filter -A DOCKER-USER -j DROP"));

    // iptables-persistent with a complete rules file is fine; with a rule missing it says which
    let live = s.host.read_to_string(&s.state.volatile().join("iptables.raw")).unwrap();
    s.host.create_dir_all(Path::new("/etc/iptables"), 0o755).unwrap();
    s.host.write(Path::new("/etc/iptables/rules.v4"), live.as_bytes(), 0o644).unwrap();
    assert!(has(&s.audit(), "reboot", Level::Ok, "live iptables rules match /etc/iptables/rules.v4"));
    let partial = live.replace("-A INPUT -p tcp -m tcp --dport 22 -j ACCEPT\n", "");
    s.host.write(Path::new("/etc/iptables/rules.v4"), partial.as_bytes(), 0o644).unwrap();
    let audit = s.audit();
    assert!(has(&audit, "reboot", Level::Gap, "1 live iptables rule(s) are not in /etc/iptables/rules.v4"));
    assert!(audit.markdown(1).contains("```\nfilter -A INPUT -p tcp -m tcp --dport 22 -j ACCEPT\n```"));
}

#[test]
fn a_container_without_restart_policy_is_a_gap_unless_something_starts_its_stack_at_boot() {
    let s = server("audit-restart");
    s.change_container("radarr", |c| c["HostConfig"]["RestartPolicy"]["Name"] = serde_json::json!("no"));
    s.change_container("dozzle", |c| c["HostConfig"]["RestartPolicy"]["Name"] = serde_json::json!(""));
    // moi.toml lists both stack roots under boot.compose_up
    let audit = s.audit();
    assert!(has(
        &audit,
        "reboot",
        Level::Ok,
        "container **radarr** has no restart policy, but its stack `/opt/yams` is started at boot by moi (`boot.compose_up`)"
    ));
    assert!(has(
        &audit,
        "reboot",
        Level::Ok,
        "container **dozzle** has no restart policy, but its stack `/opt/services/dozzle` is started at boot by moi"
    ));

    // without moi's boot tasks, the legacy units still do it: yams.service for /opt/yams, homelab-up.sh for /opt/services/*
    s.config(|t| t.replace("compose_up = [\"/opt/services/**\", \"/opt/yams\"]\n", ""));
    let audit = s.audit();
    assert!(
        has(&audit, "reboot", Level::Ok, "its stack `/opt/yams` is started at boot by yams.service"),
        "{:#?}",
        texts(&audit, "reboot", Level::Ok)
    );
    assert!(has(
        &audit,
        "reboot",
        Level::Ok,
        "its stack `/opt/services/dozzle` is started at boot by homelab-services.service"
    ));

    // and with neither, it is a gap
    s.recan(ENABLED, |out| out.lines().filter(|l| !l.starts_with("yams.service")).map(|l| format!("{l}\n")).collect());
    let audit = s.audit();
    assert_eq!(
        texts(&audit, "reboot", Level::Gap),
        ["container **radarr** is running with restart policy \"no\": it will not come back after a reboot"]
    );
}

#[test]
fn services_mounts_and_docker_that_would_not_come_back() {
    let s = server("audit-reboot-gaps");
    // chrony was started by hand and is disabled; nothing triggers it
    s.recan("systemctl list-unit-files --type=service --no-legend --no-pager", |out| {
        format!("{out}chrony.service                           disabled   enabled\n")
    });
    // the data disk was mounted by hand
    let fstab = s
        .host
        .read_to_string(Path::new("/etc/fstab"))
        .unwrap()
        .replace("LABEL=storage /mnt/storage ext4 defaults,nofail 0 2\n", "");
    s.host.write(Path::new("/etc/fstab"), fstab.as_bytes(), 0o644).unwrap();
    // docker and the Incus mount helper are not enabled
    s.recan(ENABLED, |out| {
        out.lines()
            .filter(|l| !l.starts_with("docker.service") && !l.starts_with("incus-startup"))
            .map(|l| format!("{l}\n"))
            .collect()
    });
    let audit = s.audit();
    let gaps = texts(&audit, "reboot", Level::Gap);
    assert!(
        gaps.contains(
            &"service **chrony.service** is running but disabled (started by hand?): it will not start on boot"
                .to_string()
        ),
        "{gaps:#?}"
    );
    assert!(gaps.contains(&"**/mnt/storage** (/dev/sda1, ext4) is mounted but not in /etc/fstab: it will not be mounted after a reboot".to_string()));
    assert!(gaps.contains(&"docker.service is not enabled: no container starts after a reboot".to_string()));
    assert!(has(
        &audit,
        "reboot",
        Level::Review,
        "**/var/lib/incus/storage-pools/default** is a loop-device mount (btrfs)"
    ));
    // ssh is still fine
    assert!(!gaps.iter().any(|g| g.contains("ssh.service")));
}

// ------------------------------------------------------------------ coverage

#[test]
fn without_a_backup_everything_that_holds_data_is_a_gap_except_what_is_fungible() {
    let s = server("audit-no-backup");
    s.config(|t| {
        let start = t.find("[backup]").unwrap();
        let end = t.find("# ------------------------------------------------------------------ restore").unwrap();
        format!("{}{}", &t[..start], &t[end..])
    });
    let audit = s.audit();
    let gaps = texts(&audit, "docker", Level::Gap);
    for expected in [
        "`/var/lib/docker/volumes/stellar-data/_data` — volume stellar-data → swarm service stellar-planner-x7k2: not in the backup",
        "`/var/lib/docker/volumes/dokploy-docker-config/_data` — volume dokploy-docker-config → swarm service dokploy: not in the backup",
        "`/opt/yams` — compose stack dir (yams): not in the backup",
        "`/mnt/storage/outline/data` — bind mount → outline:/var/lib/outline/data: not in the backup",
        "`/opt/services/outline/pgdata` — bind mount → outline-postgres:/var/lib/postgresql: database files of outline-postgres, and `backup.databases` does not dump it",
    ] {
        assert!(gaps.contains(&expected.to_string()), "{expected}\nnot in {gaps:#?}");
    }
    // fungible stays fungible, read-only views stay quiet
    assert!(!gaps.iter().any(|g| g.contains("/mnt/storage/media") || g.contains("/var/log/")));
    assert_eq!(texts(&audit, "docker", Level::Fungible).len(), 2);
    assert!(has(&audit, "large", Level::Info, "backups are not configured"));
    assert!(audit.markdown(1).contains("| 3.0 GB | /var/lib/docker/volumes | ✗ docker volumes not in the backup: dokploy-docker-config, dokploy-postgres-database, shortless-redis-m3n4-data, stellar-data |"));
}

#[test]
fn explicit_volume_and_database_lists_leave_the_rest_uncovered() {
    let s = server("audit-explicit-lists");
    s.config(|t| {
        t.replace("databases = \"auto\"", "databases = [\"outline-postgres\"]")
            .replace("volumes = \"auto\"", "volumes = [\"stellar-data\"]")
    });
    let audit = s.audit();
    let gaps = texts(&audit, "docker", Level::Gap);
    assert!(gaps.iter().any(|g| g.contains("dokploy-docker-config") && g.ends_with("not in the backup")), "{gaps:#?}");
    assert!(!gaps.iter().any(|g| g.contains("stellar-data")));
    assert!(gaps.iter().any(|g| g.contains("database files of bookorbit-db, and `backup.databases` does not dump it")));
    assert!(has(&audit, "docker", Level::Review, "covered by the dump of outline-postgres"));
}

#[test]
fn a_modified_package_config_is_a_gap_only_when_nothing_restores_it() {
    let s = server("audit-conffiles");
    s.config(|t| {
        t.replace(
            "etc_paths = [\"/etc/dokploy\", \"/etc/systemd/resolved.conf\"]",
            "etc_paths = []\nrestore_modified_conffiles = false",
        )
    });
    let audit = s.audit();
    assert!(has(
        &audit,
        "config",
        Level::Gap,
        "`/etc/systemd/resolved.conf` differs from the version its package ships and nothing restores it"
    ));
    // unmodified conffiles (sshd_config, chrony.conf) and obsolete entries are never listed
    assert!(!audit.markdown(1).contains("chrony.conf") && !audit.markdown(1).contains("/etc/ssh/moduli"));

    s.config(|t| t.replace("restore_modified_conffiles = false", "restore_modified_conffiles = true"));
    assert!(has(
        &s.audit(),
        "config",
        Level::Ok,
        "the restore puts it back (`restore_modified_conffiles`, showing the diff)"
    ));
}

// ------------------------------------------------------------------ how it runs

#[test]
fn sizing_only_ever_runs_at_idle_priority_and_the_audit_changes_nothing() {
    let s = server("audit-read-only");
    s.audit();
    let commands = s.fixture.commands_run();
    let sizing: Vec<&String> = commands.iter().filter(|c| c.contains(" du ")).collect();
    assert_eq!(sizing.len(), 5, "{sizing:#?}");
    assert!(sizing.iter().all(|c| c.starts_with("nice -n 19 ionice -c3 du -x -B1 -d 2 /")), "{sizing:#?}");
    assert!(!commands.iter().any(|c| c.starts_with("du ")), "du must never run at normal priority");
    assert!(s.fixture.docker_requests().iter().all(|r| r.starts_with("GET ")));
    // written: the state dir, and the two things the configuration asks for — the context copy and the
    // Claude Code pointers. Nothing else.
    let mut written = Vec::new();
    let mut stack = vec![s.overlay.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(&s.overlay).unwrap().to_string_lossy().into_owned();
            if rel == "var/lib/moi" {
                continue;
            }
            if path.is_dir() { stack.push(path) } else { written.push(rel) }
        }
    }
    written.sort();
    assert_eq!(written, ["home/prabhas/.claude/CLAUDE.md", "home/prabhas/SERVER_CONTEXT.md", "root/.claude/CLAUDE.md"]);
}

#[test]
fn without_nice_and_ionice_sizes_are_skipped_not_measured_at_full_priority() {
    let overlay = common::scratch("audit-no-nice");
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("minimal")).unwrap().with_overlay(&overlay).unwrap());
    let host: Arc<dyn Host> = fixture.clone();
    let state = StateDir::new("/var/lib/moi");
    let loaded = config::load(host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let config = Arc::new(loaded.config);
    state.ensure(host.as_ref()).unwrap();
    // no snapshot yet: the audit says what to do
    assert_eq!(
        audit::run(&host, &config, &state).unwrap_err().to_string(),
        "no snapshot yet: run `moi snapshot` first (the audit works from it)"
    );
    host.create_dir_all(&state.snapshot(), 0o700).unwrap();
    host.create_dir_all(Path::new("/opt"), 0o755).unwrap();
    let audit = audit::run(&host, &config, &state).unwrap();
    assert!(has(&audit, "large", Level::Info, "directory sizes were not measured"), "{:#?}", audit.sections[1]);
    assert!(!fixture.commands_run().iter().any(|c| c.starts_with("du")));
    // an empty server has no gaps
    assert_eq!(audit.gaps(), 0, "{}", audit.markdown(1));
}

#[test]
fn moi_audit_prints_gaps_exits_1_and_takes_a_snapshot_if_there_is_none() {
    let s = server("audit-cli");
    let mut out = Vec::new();
    let err = cli::execute(&cli::parse(&["audit"]).unwrap(), s.host.clone(), &mut out).unwrap_err();
    assert_eq!(err.exit_code(), 1);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("Docker data a rebuild could lose\n")
            && text.contains("\n  ✗ container **supabase-pooler** has 350 MB written"),
        "{text}"
    );
    assert!(text.contains("\n  review "), "{text}");
    assert!(text.ends_with("5 gap(s), 19 to review — full report: /var/lib/moi/state/audit.md\n"), "{text}");
    assert!(s.host.exists(&s.state.snapshot().join("meta/hostname")), "the audit took the missing snapshot itself");

    // --quiet keeps the gaps only
    let mut out = Vec::new();
    cli::execute(&cli::parse(&["audit", "--quiet"]).unwrap(), s.host.clone(), &mut out).unwrap_err();
    let quiet = String::from_utf8(out).unwrap();
    assert!(quiet.contains("  ✗ ") && !quiet.contains("  review "), "{quiet}");

    let mut out = Vec::new();
    cli::execute(&cli::parse(&["audit", "--json"]).unwrap(), s.host.clone(), &mut out).unwrap_err();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        (v["ok"].as_bool(), v["gaps"].as_u64(), v["file"].as_str()),
        (Some(true), Some(5), Some("/var/lib/moi/state/audit.md"))
    );
    let ids: Vec<&str> = v["audit"]["sections"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        ["reboot", "large", "docker", "config", "state", "placement", "secrets", "listeners", "monitoring"]
    );
    assert_eq!(
        v["audit"]["sections"][0]["blocks"][0],
        serde_json::json!({ "kind": "finding", "level": "ok", "text": "service **ssh.service** is disabled but started on demand by ssh.socket" })
    );
}
