//! Snapshot engine and collectors (docs/spec/02-snapshot.md) on the `cestmoi` fixture host.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cestmoi::config::{self, Config, Paths};
use cestmoi::docker::Docker;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::snapshot::{self, Collector, Ctx, Options, Output, Report};
use cestmoi::state::StateDir;

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
    config: Arc<Config>,
    state: StateDir,
    overlay: PathBuf,
}

fn server(test: &str) -> Server {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    let loaded = config::load(fixture.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let state = StateDir::new(loaded.config.state.dir.clone());
    Server { host: fixture.clone(), fixture, config: Arc::new(loaded.config), state, overlay }
}

impl Server {
    fn snapshot(&self) -> Report {
        self.state.ensure(self.host.as_ref()).unwrap();
        let db = self.state.open_db(self.host.as_ref()).unwrap();
        snapshot::run(&self.host, &self.config, &self.state, &db).unwrap()
    }

    fn snapshot_with(&self, collectors: &[Arc<dyn Collector>], timeout: Duration) -> Report {
        self.state.ensure(self.host.as_ref()).unwrap();
        let db = self.state.open_db(self.host.as_ref()).unwrap();
        snapshot::run_with(&self.host, &self.config, &self.state, &db, collectors, &Options { timeout }).unwrap()
    }

    fn read(&self, rel: &str) -> String {
        self.host.read_to_string(&self.state.state().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn has(&self, rel: &str) -> bool {
        self.host.exists(&self.state.state().join(rel))
    }

    fn write(&self, path: &str, text: &str) {
        self.host.write(Path::new(path), text.as_bytes(), 0o644).unwrap();
    }

    fn manifest_line(&self, path: &str) -> Option<String> {
        self.read("snapshot/files.manifest.tsv")
            .lines()
            .find(|l| l.split('\t').nth(3) == Some(path))
            .map(str::to_string)
    }
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

// ------------------------------------------------------------------ engine

#[test]
fn every_collector_succeeds_and_nothing_is_written_outside_the_state_dir() {
    let s = server("snap-clean");
    let report = s.snapshot();
    assert_eq!(report.failed().count(), 0, "{:?}", report.collectors);
    assert!(report.stable_files > 60, "{}", report.stable_files);
    // the overlay holds every write made to the host: only the state dir may appear
    assert_eq!(std::fs::read_dir(&s.overlay).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>(), ["var"]);
    assert_eq!(
        std::fs::read_dir(s.overlay.join("var/lib")).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>(),
        ["moi"]
    );
    // only read-only tools run, and none of the package managers that write logs or caches
    let allowed = [
        "systemctl",
        "systemd-detect-virt",
        "ip",
        "resolvectl",
        "ss",
        "iptables-save",
        "ip6tables-save",
        "nft",
        "ipset",
        "ufw",
        "fail2ban-client",
        "cscli",
        "tailscale",
        "wg",
        "lsblk",
        "blkid",
        "mdadm",
        "pvs",
        "vgs",
        "lvs",
        "zpool",
        "btrfs",
        "df",
        "smartctl",
        "incus",
    ];
    for line in s.fixture.commands_run() {
        let program = line.split(' ').next().unwrap();
        assert!(allowed.contains(&program), "unexpected command: {line}");
    }
}

#[test]
fn a_second_snapshot_changes_nothing_and_reads_no_file_again() {
    let s = server("snap-idempotent");
    let first = s.snapshot();
    assert!(first.files_read > 40, "{}", first.files_read);
    assert!(!first.changed.is_empty());
    let second = s.snapshot();
    assert_eq!(second.changed, Vec::<String>::new());
    assert_eq!(second.files_read, 0, "unchanged files must come from the cache");
    assert_eq!(second.stable_files, first.stable_files);
}

#[test]
fn editing_a_file_in_etc_changes_exactly_that_copy_and_the_manifest() {
    let s = server("snap-edit-etc");
    s.snapshot();
    s.write("/etc/hosts", "127.0.0.1 localhost\n192.168.1.10 cestmoi\n");
    let report = s.snapshot();
    assert_eq!(report.changed, ["snapshot/files.manifest.tsv", "snapshot/files/etc/hosts"]);
    assert_eq!(report.files_read, 1);
    assert_eq!(s.read("snapshot/files/etc/hosts"), "127.0.0.1 localhost\n192.168.1.10 cestmoi\n");
}

#[test]
fn a_permission_change_alone_is_recorded_in_the_manifest() {
    let s = server("snap-chmod");
    s.write("/etc/motd", "hello\n");
    s.snapshot();
    let before = s.manifest_line("/etc/motd").unwrap();
    assert!(before.starts_with("644\troot\troot\t/etc/motd\t6\t"), "{before}");
    s.host.set_mode(Path::new("/etc/motd"), 0o600).unwrap();
    let report = s.snapshot();
    assert_eq!(report.changed, ["snapshot/files.manifest.tsv"]);
    assert!(s.manifest_line("/etc/motd").unwrap().starts_with("600\t"));
}

#[test]
fn a_file_removed_from_the_host_is_removed_from_the_snapshot() {
    let s = server("snap-remove");
    s.write("/etc/motd", "hello\n");
    s.host.create_dir_all(Path::new("/etc/motd.d"), 0o755).unwrap();
    s.write("/etc/motd.d/10-extra", "more\n");
    s.snapshot();
    assert!(s.has("snapshot/files/etc/motd.d/10-extra"));
    s.host.remove_file(Path::new("/etc/motd.d/10-extra")).unwrap();
    s.host.remove_file(Path::new("/etc/motd")).unwrap();
    let report = s.snapshot();
    assert_eq!(
        report.changed,
        ["snapshot/files.manifest.tsv", "snapshot/files/etc/motd", "snapshot/files/etc/motd.d/10-extra"]
    );
    assert!(!s.has("snapshot/files/etc/motd"));
    assert!(!s.has("snapshot/files/etc/motd.d"), "empty directories are removed too");
    assert_eq!(s.manifest_line("/etc/motd"), None);
}

#[test]
fn files_over_max_file_are_listed_with_their_hash_but_not_copied() {
    let s = server("snap-big");
    let big = "x".repeat(1024 * 1024 + 1);
    s.write("/etc/big.dat", &big);
    let report = s.snapshot();
    assert_eq!(report.failed().count(), 0);
    assert!(!s.has("snapshot/files/etc/big.dat"));
    let line = s.manifest_line("/etc/big.dat").unwrap();
    let fields: Vec<&str> = line.split('\t').collect();
    assert_eq!(fields[4], "1048577");
    assert_eq!(fields[5].len(), 64, "{line}");
    // …and it is not hashed again while it stays the same
    assert_eq!(s.snapshot().files_read, 0);
}

struct Fake {
    name: &'static str,
    behaviour: fn() -> Result<Output, String>,
}

impl Collector for Fake {
    fn name(&self) -> &'static str {
        self.name
    }
    fn collect(&self, _: &Ctx) -> Result<Output, String> {
        (self.behaviour)()
    }
}

fn works() -> Result<Output, String> {
    let mut out = Output::default();
    out.stable("probe/value.txt", "1\n");
    out.volatile("probe-now.txt", "now\n");
    out.grab("/etc/hostname");
    Ok(out)
}

#[test]
fn a_failing_collector_keeps_its_previous_output_and_is_reported_stale() {
    let s = server("snap-failing");
    let timeout = Duration::from_secs(30);
    s.snapshot_with(&[Arc::new(Fake { name: "probe", behaviour: works })], timeout);
    assert_eq!(s.read("snapshot/probe/value.txt"), "1\n");
    assert!(!s.has("volatile/collector-errors.json"));

    let report =
        s.snapshot_with(&[Arc::new(Fake { name: "probe", behaviour: || Err("engine not reachable".into()) })], timeout);
    assert_eq!(
        report.failed().map(|c| (c.name.as_str(), c.error.as_deref())).collect::<Vec<_>>(),
        [("probe", Some("engine not reachable"))]
    );
    assert_eq!(report.changed, Vec::<String>::new(), "a failure is not a change");
    assert_eq!(s.read("snapshot/probe/value.txt"), "1\n");
    assert_eq!(s.read("snapshot/files/etc/hostname"), "cestmoi\n");
    assert!(s.manifest_line("/etc/hostname").is_some());
    let errors: serde_json::Value = serde_json::from_str(&s.read("volatile/collector-errors.json")).unwrap();
    assert_eq!(errors, serde_json::json!([{ "collector": "probe", "error": "engine not reachable", "stale": true }]));

    // once it works again, the error file goes away
    s.snapshot_with(&[Arc::new(Fake { name: "probe", behaviour: works })], timeout);
    assert!(!s.has("volatile/collector-errors.json"));
}

#[test]
fn a_collector_that_times_out_or_panics_does_not_delete_its_previous_files() {
    let s = server("snap-timeout");
    s.snapshot_with(&[Arc::new(Fake { name: "probe", behaviour: works })], Duration::from_secs(30));
    let slow: Arc<dyn Collector> = Arc::new(Fake {
        name: "probe",
        behaviour: || {
            std::thread::sleep(Duration::from_secs(3));
            Ok(Output::default())
        },
    });
    let started = std::time::Instant::now();
    let report = s.snapshot_with(
        &[slow, Arc::new(Fake { name: "other", behaviour: || panic!("boom") })],
        Duration::from_millis(200),
    );
    assert!(started.elapsed() < Duration::from_secs(2), "the snapshot must not wait for a stuck collector");
    let errors: Vec<_> = report.collectors.iter().map(|c| (c.name.as_str(), c.error.clone().unwrap())).collect();
    assert_eq!(errors, [("probe", "timed out after 0s".to_string()), ("other", "collector panicked".to_string())]);
    assert_eq!(s.read("snapshot/probe/value.txt"), "1\n");
    assert!(s.has("snapshot/files/etc/hostname"));
}

#[test]
fn output_a_collector_stops_producing_is_removed() {
    let s = server("snap-stops");
    s.snapshot_with(&[Arc::new(Fake { name: "probe", behaviour: works })], Duration::from_secs(30));
    let report = s.snapshot_with(
        &[Arc::new(Fake { name: "probe", behaviour: || Ok(Output::default()) })],
        Duration::from_secs(30),
    );
    assert_eq!(
        report.changed,
        ["snapshot/files.manifest.tsv", "snapshot/files/etc/hostname", "snapshot/probe/value.txt"]
    );
    assert!(!s.has("snapshot/probe") && !s.has("volatile/probe-now.txt"));
}

#[test]
fn volatile_files_never_count_as_changes() {
    let s = server("snap-volatile");
    s.snapshot();
    s.fixture.advance(Duration::from_secs(3600));
    let report = s.snapshot();
    assert_eq!(report.changed, Vec::<String>::new());
    assert_eq!(s.read("volatile/captured-at"), "2026-11-26T18:00:00Z\n");
}

// ------------------------------------------------------------------ collectors

#[test]
fn meta_describes_the_machine_without_values_that_drift() {
    let s = server("snap-meta");
    s.snapshot();
    assert_eq!(s.read("snapshot/meta/hostname"), "cestmoi\n");
    assert_eq!(s.read("snapshot/meta/kernel"), "Linux 7.0.0-14-generic x86_64\n");
    assert_eq!(
        s.read("snapshot/meta/cpu"),
        "model: Intel(R) Core(TM) i5-9500T CPU @ 2.20GHz\nsockets: 1\ncores per socket: 6\nthreads: 6\n"
    );
    assert_eq!(s.read("snapshot/meta/memory"), "MemTotal:       16163412 kB\nSwapTotal:       4194300 kB\n");
    assert_eq!(lines(&s.read("snapshot/meta/dmi"))[..2], ["sys_vendor: LENOVO", "product_name: 10RRS76200"]);
    assert_eq!(s.read("snapshot/meta/timezone"), "America/New_York\n");
    assert_eq!(s.read("snapshot/meta/locale"), "en_US.UTF-8\n");
    assert!(s.read("snapshot/meta/os-release").contains("Ubuntu 26.04.1 LTS"));
    assert!(!s.has("snapshot/meta/virt"), "systemd-detect-virt exits 1 on bare metal: no file");
    assert!(s.read("volatile/uptime").starts_with("432123.45"));
}

#[test]
fn packages_come_from_state_files() {
    let s = server("snap-packages");
    s.snapshot();
    assert_eq!(s.read("snapshot/packages/family"), "debian\n");
    // installed, not auto-installed; the removed-but-configured nano is not installed
    assert_eq!(
        lines(&s.read("snapshot/packages/apt-manual.txt")),
        ["bash", "chrony", "docker-compose-v2", "docker.io", "incus", "linux-image-generic", "restic", "smartmontools"]
    );
    assert_eq!(s.read("snapshot/packages/apt-hold.txt"), "linux-image-generic\n");
    assert_eq!(lines(&s.read("volatile/apt-all.tsv")).len(), 12);
    assert_eq!(s.read("snapshot/packages/snap.txt"), "core22\n");
    assert_eq!(s.read("snapshot/packages/pipx.txt"), "yt-dlp\n");
    assert_eq!(s.read("snapshot/packages/npm-global.txt"), "@anthropic-ai/claude-code\n");
    assert_eq!(s.read("snapshot/packages/cargo.txt"), "ripgrep 14.1.1\n");
    assert_eq!(
        lines(&s.read("snapshot/packages/usr-local-bin.tsv")),
        [
            "/usr/local/bin/ctop\tELF executable",
            "/usr/local/bin/serverstate\tscript (/usr/bin/env bash)",
            "/usr/local/bin/yt-dlp\tscript (/root/.local/share/pipx/venvs/yt-dlp/bin/python)",
            "/usr/local/sbin/host-firewall\tscript (/bin/bash)",
        ]
    );
    assert_eq!(lines(&s.read("snapshot/packages/opt-listing.txt")), ["scripts", "services", "yams"]);
    assert!(s.has("snapshot/files/usr/share/keyrings/ubuntu-archive-keyring.gpg"));
}

#[test]
fn etc_is_copied_verbatim_minus_exclusions() {
    let s = server("snap-etc");
    s.snapshot();
    assert_eq!(s.read("snapshot/files/etc/systemd/resolved.conf"), "[Resolve]\nDNSStubListener=no\nDNS=127.0.0.1\n");
    assert!(s.has("snapshot/files/etc/dokploy/traefik/traefik.yml"));
    for excluded in ["etc/dokploy/applications", "etc/dokploy/logs", "etc/ssl/certs", "etc/ld.so.cache"] {
        assert!(!s.has(&format!("snapshot/files/{excluded}")), "{excluded}");
    }
    // symlinks stay symlinks, with their target in the manifest
    let link = s.state.state().join("snapshot/files/etc/localtime");
    assert_eq!(s.host.read_link(&link).unwrap(), Path::new("/usr/share/zoneinfo/America/New_York"));
    assert_eq!(
        s.manifest_line("/etc/localtime").unwrap(),
        "777\troot\troot\t/etc/localtime\t36\t-> /usr/share/zoneinfo/America/New_York"
    );
    // owners and modes are in the manifest, the copies themselves are private
    assert!(s.manifest_line("/etc/shadow").unwrap().starts_with("640\troot\tshadow\t/etc/shadow\t"));
    assert_eq!(s.host.metadata(&s.state.state().join("snapshot/files/etc/shadow")).unwrap().mode, 0o600);
}

#[test]
fn users_and_their_dotfiles() {
    let s = server("snap-users");
    s.snapshot();
    assert_eq!(
        lines(&s.read("snapshot/users/users.passwd")),
        ["root:x:0:0:root:/root:/bin/bash", "prabhas:x:1000:1000:Prabhas:/home/prabhas:/bin/bash"]
    );
    assert_eq!(
        lines(&s.read("snapshot/users/groups.group")),
        ["sudo:x:27:prabhas", "docker:x:988:prabhas", "incus-admin:x:989:prabhas", "prabhas:x:1000:"]
    );
    for f in [".bashrc", ".profile", ".gitconfig", ".ssh/authorized_keys", ".config/rclone/rclone.conf"] {
        assert!(s.has(&format!("snapshot/files/home/prabhas/{f}")), "{f}");
    }
    assert!(s.has("snapshot/files/root/.bashrc"));
    assert!(!s.has("snapshot/files/home/prabhas/notes.txt"), "only the listed dotfiles are taken from a home");
    assert!(s.manifest_line("/home/prabhas/.ssh/authorized_keys").unwrap().starts_with("600\tprabhas\tprabhas\t"));
}

#[test]
fn cron_and_timers() {
    let s = server("snap-cron");
    s.snapshot();
    assert!(s.read("snapshot/cron/user-prabhas.crontab").contains("/home/prabhas/bin/weekly-note.sh"));
    assert!(s.manifest_line("/var/spool/cron/crontabs/prabhas").unwrap().starts_with("600\tprabhas\tcrontab\t"));
    assert_eq!(
        lines(&s.read("snapshot/cron/enabled-timers.txt")),
        ["server-backup.timer", "homelab-selfcheck.timer", "apt-daily.timer"]
    );
    assert!(s.read("volatile/systemd-timers-schedule.txt").contains("server-backup.timer"));
    assert!(s.has("snapshot/files/etc/cron.d/storage-check"));
}

#[test]
fn adding_a_cron_line_is_one_small_change() {
    let s = server("snap-cron-change");
    s.snapshot();
    let path = "/var/spool/cron/crontabs/prabhas";
    let old = s.host.read_to_string(Path::new(path)).unwrap();
    s.host
        .write(
            Path::new(path),
            format!("{old}30 2 * * * /home/prabhas/bin/weekly-note.sh --nightly\n").as_bytes(),
            0o600,
        )
        .unwrap();
    let report = s.snapshot();
    assert_eq!(
        report.changed,
        [
            "snapshot/cron/user-prabhas.crontab",
            "snapshot/files.manifest.tsv",
            "snapshot/files/var/spool/cron/crontabs/prabhas"
        ]
    );
}

#[test]
fn systemd_units_owned_by_the_owner() {
    let s = server("snap-systemd");
    s.snapshot();
    let custom = s.read("snapshot/systemd/custom-units.txt");
    let custom = lines(&custom);
    assert_eq!(custom.len(), 13, "{custom:#?}");
    assert!(custom.contains(&"/etc/systemd/system/docker.service.d/wait-for-storage.conf"));
    assert!(custom.contains(&"/etc/systemd/system/traefik-config-guard.path"));
    assert!(!custom.iter().any(|u| u.contains("snap-") || u.contains(".wants/")), "{custom:#?}");
    assert_eq!(lines(&s.read("snapshot/systemd/enabled-units.txt"))[..2], ["chrony.service", "cron.service"]);
    assert!(s.read("snapshot/systemd/unit-file-states.txt").contains("ssh.service disabled\n"));
    // socket activation: ssh.service is disabled but started by its socket
    assert_eq!(s.read("snapshot/systemd/triggered-by.txt"), "ssh.service ssh.socket\n");
    assert!(s.read("volatile/running-services.txt").contains("docker.service"));
    assert!(!s.has("volatile/failed-units.txt"));
}

#[test]
fn firewall_stable_rules_drop_counters_docker_chains_and_bans() {
    let s = server("snap-firewall");
    s.snapshot();
    assert_eq!(
        lines(&s.read("snapshot/firewall/iptables.rules")),
        [
            "*filter",
            ":INPUT DROP",
            ":FORWARD DROP",
            ":OUTPUT ACCEPT",
            ":DOCKER-USER -",
            ":f2b-sshd -",
            "-A INPUT -i lo -j ACCEPT",
            "-A INPUT -p tcp -m tcp --dport 22 -j ACCEPT",
            "-A INPUT -p tcp -m multiport --dports 22 -j f2b-sshd",
            "-A FORWARD -j DOCKER-USER",
            "-A DOCKER-USER -m comment --comment \"Intra-network container traffic\" -j RETURN",
            "-A DOCKER-USER -m comment --comment \"Swarm outbound Internet\" -j RETURN",
            "-A DOCKER-USER -j DROP",
            "-A f2b-sshd -j RETURN",
            "COMMIT",
        ]
    );
    assert!(s.read("volatile/iptables.raw").contains("203.0.113.7"), "the raw rules keep everything");
    assert_eq!(lines(&s.read("snapshot/firewall/ip6tables.rules"))[1], ":INPUT DROP");
    assert!(
        !s.has("snapshot/firewall/managers.txt"),
        "no firewall manager is active on cestmoi: its own unit applies the rules"
    );
}

#[test]
fn network_without_container_interfaces() {
    let s = server("snap-network");
    s.snapshot();
    let addresses = s.read("snapshot/network/addresses.txt");
    assert_eq!(
        lines(&addresses).iter().map(|l| l.split_whitespace().next().unwrap()).collect::<Vec<_>>(),
        ["lo", "enp1s0", "incusbr0"]
    );
    assert_eq!(lines(&s.read("snapshot/network/links.txt")).len(), 3);
    assert_eq!(
        lines(&s.read("snapshot/network/routes.txt")),
        [
            "default via 192.168.1.1 dev enp1s0 proto static",
            "10.143.9.0/24 dev incusbr0 proto kernel scope link src 10.143.9.1",
            "192.168.1.0/24 dev enp1s0 proto kernel scope link src 192.168.1.10",
        ]
    );
    assert_eq!(lines(&s.read("snapshot/network/dns.txt")), ["Global: 127.0.0.1", "Link 2 (enp1s0): 192.168.1.1"]);
    assert_eq!(
        lines(&s.read("volatile/listening.txt")),
        [
            "tcp 0.0.0.0:22 users:((\"sshd\"),(\"systemd\"))",
            "tcp 0.0.0.0:80 users:((\"docker-proxy\"))",
            "udp 0.0.0.0:53 users:((\"AdGuardHome\"))",
        ]
    );
}

#[test]
fn storage_mounts_devices_and_data_roots() {
    let s = server("snap-storage");
    s.snapshot();
    assert_eq!(
        lines(&s.read("snapshot/storage/mounts.txt")),
        [
            "/ /dev/mapper/ubuntu--vg-ubuntu--lv ext4 rw,relatime",
            "/boot /dev/nvme0n1p2 ext4 rw,relatime",
            "/boot/efi /dev/nvme0n1p1 vfat rw,relatime",
            "/mnt/storage /dev/sda1 ext4 rw,relatime",
            "/var/lib/incus/storage-pools/default /dev/loop0 btrfs rw,relatime",
        ]
    );
    let lsblk: serde_json::Value = serde_json::from_str(&s.read("snapshot/storage/lsblk.json")).unwrap();
    assert_eq!(lsblk["blockdevices"][0]["children"][0]["label"], "storage");
    assert!(s.read("snapshot/storage/blkid.txt").contains("LABEL=\"storage\""));
    assert!(s.read("snapshot/storage/lvm.txt").contains("ubuntu-lv"));
    assert!(
        s.read("snapshot/storage/btrfs-subvolumes.txt").starts_with("== /var/lib/incus/storage-pools/default\nID 256")
    );
    // data roots are taken from the config on cestmoi
    assert_eq!(lines(&s.read("snapshot/storage/data-roots.txt")), ["/opt", "/mnt/storage", "/home"]);
    // smartctl's non-zero exit status (a bit mask) does not hide its output
    assert!(s.read("volatile/smart.txt").contains("== /dev/sda\nDevice Model:     TOSHIBA MQ04UBB400"));
    assert!(s.read("volatile/df.txt").contains("/mnt/storage"));
}

#[test]
fn data_roots_are_discovered_when_not_configured() {
    let s = server("snap-roots-auto");
    let text = s
        .host
        .read_to_string(Path::new("/etc/moi/moi.toml"))
        .unwrap()
        .replace("data_roots = [\"/opt\", \"/mnt/storage\", \"/home\"]\n", "");
    s.write("/etc/moi/moi.toml", &text);
    let loaded = config::load(s.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let s = Server { config: Arc::new(loaded.config), ..s };
    s.snapshot();
    // mounted data filesystems that are not the system's own, plus the usual top-level data directories
    assert_eq!(
        lines(&s.read("snapshot/storage/data-roots.txt")),
        ["/home", "/mnt", "/mnt/storage", "/opt", "/var/lib/incus/storage-pools/default"]
    );
}

#[test]
fn trees_record_directory_structure_with_owners_to_the_configured_depth() {
    let s = server("snap-trees");
    s.snapshot();
    assert_eq!(
        lines(&s.read("volatile/trees/mnt_storage.tsv")),
        [
            "755\tprabhas\tprabhas\t/mnt/storage",
            "755\troot\troot\t/mnt/storage/backups",
            "755\tprabhas\tprabhas\t/mnt/storage/database",
            "755\tprabhas\tprabhas\t/mnt/storage/database/db-config",
            "755\tprabhas\tprabhas\t/mnt/storage/media",
            "755\tprabhas\tprabhas\t/mnt/storage/media/movies",
            "755\tprabhas\tprabhas\t/mnt/storage/media/tv",
            "755\tprabhas\tprabhas\t/mnt/storage/media/tv/Show A",
            "755\tprabhas\tprabhas\t/mnt/storage/media/tv/Show A/Season 1",
            "755\troot\troot\t/mnt/storage/outline",
            "755\tprabhas\tprabhas\t/mnt/storage/outline/data",
        ],
        "depth 4 below the root, lost+found skipped"
    );
    assert!(s.read("volatile/trees/home.tsv").contains("750\tprabhas\tprabhas\t/home/prabhas\n"));
    assert!(s.has("volatile/trees/opt.tsv"));
}

#[test]
fn scripts_include_what_cron_and_units_run_and_what_those_scripts_use() {
    let s = server("snap-scripts");
    s.snapshot();
    let referenced = s.read("snapshot/scripts/referenced.txt");
    let referenced = lines(&referenced);
    for expected in [
        "/home/prabhas/bin/weekly-note.sh", // user crontab
        "/opt/scripts/resource-check.sh",   // /etc/cron.d
        "/opt/scripts/backup.sh",           // server-backup.service
        "/opt/scripts/homelab-up.sh",       // homelab-services.service
        "/opt/scripts/lib/common.sh",       // sourced by homelab-up.sh
        "/opt/scripts/lib/notify.sh",       // named in common.sh
        "/usr/local/sbin/host-firewall",    // host-firewall.service
    ] {
        assert!(referenced.contains(&expected), "{expected} missing from {referenced:#?}");
    }
    assert!(
        !referenced.iter().any(|p| p.starts_with("/etc/") || p.starts_with("/usr/bin/") || p.starts_with("/var/log/"))
    );

    let index = s.read("snapshot/scripts/index.txt");
    let index = lines(&index);
    assert!(index.contains(&"/opt/scripts/backup.sh") && index.contains(&"/opt/scripts/lib/notify.sh"));
    assert!(index.contains(&"/usr/local/bin/serverstate"));
    assert!(!index.contains(&"/usr/local/bin/yt-dlp"), "generated console-script stubs are not the owner's scripts");
    assert!(!index.contains(&"/usr/local/bin/ctop"), "binaries are not scripts");
    assert!(!index.contains(&"/opt/scripts/README.md"));
    // the files themselves are captured, also outside the listed script dirs
    assert!(s.has("snapshot/files/opt/scripts/lib/notify.sh"));
    assert!(s.has("snapshot/files/home/prabhas/bin/weekly-note.sh"));
    assert!(s.has("snapshot/files/usr/local/bin/ctop"));
}

// ------------------------------------------------------------------ docker

fn json(s: &Server, rel: &str) -> serde_json::Value {
    serde_json::from_str(&s.read(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn by_name<'a>(list: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    list.as_array().unwrap().iter().find(|x| x["name"] == name).unwrap_or_else(|| panic!("no {name}"))
}

#[test]
fn containers_are_normalized_to_their_configuration() {
    let s = server("snap-docker-containers");
    s.snapshot();
    let containers = json(&s, "snapshot/docker/containers.json");
    let names: Vec<&str> = containers.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 35);
    assert!(names.is_sorted());
    // swarm task containers never appear: the service describes them
    assert!(!names.iter().any(|n| n.contains(".1.")), "{names:?}");
    assert!(!s.read("snapshot/docker/containers.json").contains("com.docker.swarm.task"));

    // container:<id> network modes are rewritten to names
    assert_eq!(by_name(&containers, "qbittorrent")["host_config"]["NetworkMode"], "container:gluetun");
    assert_eq!(by_name(&containers, "sabnzbd")["host_config"]["NetworkMode"], "container:gluetun");
    assert!(by_name(&containers, "qbittorrent").get("networks").is_none());

    let outline = by_name(&containers, "outline");
    assert!(outline.get("hostname").is_none(), "a hostname that is just the short id is dropped");
    assert_eq!(outline["networks"]["homelab"]["aliases"], serde_json::json!(["outline"]), "no id aliases");
    assert!(outline["env"].as_array().unwrap().is_sorted_by_key(|e| e.as_str().unwrap().to_string()));
    assert_eq!(
        outline["mounts"][0],
        serde_json::json!({ "destination": "/var/lib/outline/data", "source": "/mnt/storage/outline/data", "type": "bind" })
    );

    // anonymous volumes keep no name, read-only mounts are marked
    assert_eq!(
        by_name(&containers, "outline-redis")["mounts"][0],
        serde_json::json!({ "anonymous": true, "destination": "/data", "type": "volume" })
    );
    let logs = &by_name(&containers, "host-logs")["mounts"][0];
    assert_eq!((logs["source"].as_str(), logs["ro"].as_bool()), (Some("/var/log/server-backup.log"), Some(true)));
    // nothing that changes while running
    let text = s.read("snapshot/docker/containers.json");
    for volatile in ["StartedAt", "\"Pid\"", "IPAddress", "EndpointID", "MacAddress", "\"State\""] {
        assert!(!text.contains(volatile), "{volatile}");
    }
    assert_eq!(s.read("snapshot/docker/version"), "29.1.3\n");
    assert_eq!(s.read("snapshot/docker/compose-version"), "2.40.3\n");
    assert_eq!(s.read("snapshot/docker/root-dir"), "/var/lib/docker\n");
    assert!(s.read("volatile/docker-ps.tsv").contains("jellyfin\trunning\t"));
}

#[test]
fn networks_volumes_and_swarm_services() {
    let s = server("snap-docker-swarm");
    s.snapshot();
    let networks = json(&s, "snapshot/docker/networks.json");
    let homelab = by_name(&networks, "homelab");
    assert_eq!(homelab["subnets"], serde_json::json!([{ "gateway": "172.19.0.1", "subnet": "172.19.0.0/16" }]));
    assert_eq!((homelab["driver"].as_str(), homelab["attachable"].as_bool()), (Some("bridge"), Some(true)));
    assert_eq!(by_name(&networks, "dokploy-network")["scope"], "swarm");

    let volumes = json(&s, "snapshot/docker/volumes.json");
    let names: Vec<&str> = volumes.as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["dokploy-docker-config", "dokploy-postgres-database", "shortless-redis-m3n4-data", "stellar-data"],
        "named volumes only"
    );

    assert_eq!(s.read("snapshot/docker/swarm-state"), "active\n");
    assert_eq!(s.read("snapshot/docker/swarm-nodes.txt"), "cestmoi\tLeader\tactive\n");
    let services = json(&s, "snapshot/docker/services.json");
    assert_eq!(services.as_array().unwrap().len(), 7);
    let dokploy = by_name(&services, "dokploy");
    assert_eq!(dokploy["image"], "dokploy/dokploy:v0.31.0", "the digest is split off");
    assert!(dokploy["image_digest"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(
        dokploy["networks"],
        serde_json::json!([{ "aliases": ["dokploy"], "name": "dokploy-network" }]),
        "network ids become names"
    );
    assert_eq!(
        dokploy["ports"],
        serde_json::json!([{ "mode": "host", "protocol": "tcp", "published": 3000, "target": 3000 }])
    );
    assert_eq!(dokploy["constraints"], serde_json::json!(["node.role==manager"]));
    assert_eq!(
        by_name(&services, "stellar-planner-x7k2")["mounts"],
        serde_json::json!([{ "destination": "/app/data", "source": "stellar-data", "type": "volume" }])
    );
    assert!(s.read("volatile/service-ls.tsv").contains("dokploy\t1\tdokploy/dokploy:v0.31.0\t3000->3000\n"));
}

#[test]
fn compose_projects_come_from_labels_and_from_files_nobody_runs() {
    let s = server("snap-docker-compose");
    s.snapshot();
    let projects = json(&s, "snapshot/docker/compose-projects.json");
    let names: Vec<&str> = projects.as_array().unwrap().iter().map(|p| p["project"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "adguard",
            "cloudflared",
            "database",
            "ddns",
            "dozzle",
            "glances",
            "homepage",
            "host-logs",
            "old-wiki",
            "outline",
            "scrutiny",
            "seerr",
            "uptime-kuma",
            "yams"
        ]
    );
    let project = |name: &str| projects.as_array().unwrap().iter().find(|p| p["project"] == name).unwrap();
    let outline = project("outline");
    assert_eq!(outline["workdir"], "/opt/services/outline");
    assert_eq!(outline["config_files"], serde_json::json!(["/opt/services/outline/docker-compose.yml"]));
    assert_eq!(outline["env_files"], serde_json::json!(["/opt/services/outline/.env"]));
    assert_eq!(outline["containers"], serde_json::json!(["outline", "outline-postgres", "outline-redis"]));
    assert_eq!(outline["defined_services"], serde_json::json!(["outline", "postgres", "redis"]));
    let orphan = project("old-wiki");
    assert_eq!(orphan["source"], "found on disk (no containers)");
    assert_eq!(orphan["defined_services"], serde_json::json!(["wiki", "wiki-db"]));

    // stack directories are captured without their data
    for kept in [
        "opt/services/outline/docker-compose.yml",
        "opt/services/outline/.env",
        "opt/services/homepage/config/services.yaml",
        "opt/yams/config/sonarr/config.xml",
        "opt/services/old-wiki/docker-compose.yml",
    ] {
        assert!(s.has(&format!("snapshot/files/{kept}")), "{kept}");
    }
    for pruned in [
        "opt/services/outline/pgdata",
        "opt/yams/config/sonarr/logs",
        "opt/yams/config/jellyfin/data",
        "opt/services/monitoring/uptime-kuma/data",
    ] {
        assert!(!s.has(&format!("snapshot/files/{pruned}")), "{pruned}");
    }
}

#[test]
fn a_standalone_container_gets_a_run_script_to_review() {
    let s = server("snap-docker-standalone");
    s.snapshot();
    assert_eq!(s.host.read_dir(&s.state.state().join("snapshot/docker/standalone")).unwrap(), ["dokploy-traefik.sh"]);
    assert_eq!(
        s.read("snapshot/docker/standalone/dokploy-traefik.sh"),
        "#!/bin/sh\n\
         # Recreate standalone container \"dokploy-traefik\": generated by moi from the Docker API.\n\
         # Review before running. Better: move it into a compose file.\n\
         set -e\n\
         docker run -d \\\n  --name dokploy-traefik \\\n  --restart always \\\n  --network dokploy-network \\\n  -p 443:443 \\\n  -p 80:80 \\\n  \
         -v /etc/dokploy/traefik/dynamic:/etc/dokploy/traefik/dynamic \\\n  -v /etc/dokploy/traefik/traefik.yml:/etc/traefik/traefik.yml \\\n  \
         -v /var/run/docker.sock:/var/run/docker.sock:ro \\\n  -e CF_DNS_API_TOKEN=FAKEcftraefikdnstoken0000 \\\n  \
         --log-opt max-file=3 \\\n  --log-opt max-size=10m \\\n  traefik:v3.6.1\n\
         docker network connect homelab dokploy-traefik\n"
    );
}

fn change_container(s: &Server, name: &str, change: impl FnOnce(&mut serde_json::Value)) {
    let mut c = Docker::new(s.host.clone()).container(name).unwrap();
    change(&mut c);
    s.fixture.set_docker(&format!("/containers/{}/json", c["Id"].as_str().unwrap()), &c.to_string());
}

#[test]
fn changing_a_containers_env_changes_only_the_container_list() {
    let s = server("snap-docker-env-change");
    s.snapshot();
    change_container(&s, "sonarr", |c| {
        c["Config"]["Env"].as_array_mut().unwrap().push(serde_json::json!("SONARR__LOG__LEVEL=debug"))
    });
    let report = s.snapshot();
    assert_eq!(report.changed, ["snapshot/docker/containers.json"]);
    // a restart (new pid, new start time, new address) is not a change
    change_container(&s, "radarr", |c| {
        c["State"]["StartedAt"] = serde_json::json!("2026-11-26T16:59:00Z");
        c["State"]["Pid"] = serde_json::json!(99_999);
        c["NetworkSettings"]["Networks"]["homelab"]["IPAddress"] = serde_json::json!("172.19.0.99");
    });
    assert_eq!(s.snapshot().changed, Vec::<String>::new());
}

#[test]
fn recreating_the_container_whose_network_is_shared_is_not_a_change() {
    let s = server("snap-docker-recreate");
    s.snapshot();
    // gluetun comes back with a new id; qbittorrent's NetworkMode follows it
    let docker = Docker::new(s.host.clone());
    let mut list = docker.containers(true).unwrap();
    let old_id = docker.container("gluetun").unwrap()["Id"].as_str().unwrap().to_string();
    let new_id = "9".repeat(64);
    for summary in &mut list {
        if summary["Id"] == old_id.as_str() {
            summary["Id"] = serde_json::json!(new_id);
        }
    }
    let mut gluetun = docker.container("gluetun").unwrap();
    gluetun["Id"] = serde_json::json!(new_id);
    gluetun["Config"]["Hostname"] = serde_json::json!(&new_id[..12]);
    s.fixture.set_docker("/containers/json", &serde_json::Value::Array(list).to_string());
    s.fixture.set_docker(&format!("/containers/{new_id}/json"), &gluetun.to_string());
    for name in ["qbittorrent", "sabnzbd"] {
        change_container(&s, name, |c| {
            c["HostConfig"]["NetworkMode"] = serde_json::json!(format!("container:{new_id}"))
        });
    }
    assert_eq!(s.snapshot().changed, Vec::<String>::new());
}

#[test]
fn creating_a_swarm_service_changes_only_the_service_list() {
    let s = server("snap-docker-new-service");
    s.snapshot();
    let mut services = Docker::new(s.host.clone()).services().unwrap();
    let mut new = services[0].clone();
    new["Spec"]["Name"] = serde_json::json!("blog-z1y2");
    services.push(new);
    s.fixture.set_docker("/services", &serde_json::Value::Array(services).to_string());
    assert_eq!(s.snapshot().changed, ["snapshot/docker/services.json"]);
    assert_eq!(json(&s, "snapshot/docker/services.json").as_array().unwrap().len(), 8);
}

#[test]
fn an_engine_that_does_not_answer_keeps_the_previous_docker_and_monitoring_state() {
    let s = server("snap-docker-down");
    s.snapshot();
    let before = s.read("snapshot/docker/containers.json");
    s.fixture.set_docker("/version", "<html>502 Bad Gateway</html>");
    let report = s.snapshot();
    let failed: Vec<&str> = report.failed().map(|c| c.name.as_str()).collect();
    assert_eq!(failed, ["docker", "monitoring"]);
    assert_eq!(report.changed, Vec::<String>::new());
    assert_eq!(s.read("snapshot/docker/containers.json"), before);
    assert!(s.has("snapshot/monitoring/kuma-monitors.json") && s.has("snapshot/files/opt/yams/docker-compose.yml"));
}

#[test]
fn a_host_without_docker_or_incus_is_not_an_error() {
    let overlay = common::scratch("snap-no-docker");
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("minimal")).unwrap().with_overlay(&overlay).unwrap());
    let loaded = config::load(fixture.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let s = Server {
        host: fixture.clone(),
        fixture,
        config: Arc::new(loaded.config),
        state: StateDir::new("/var/lib/moi"),
        overlay,
    };
    let wanted = ["docker", "incus", "monitoring"];
    let collectors: Vec<_> = snapshot::collectors().into_iter().filter(|c| wanted.contains(&c.name())).collect();
    let report = s.snapshot_with(&collectors, Duration::from_secs(30));
    assert_eq!(report.failed().count(), 0, "{:?}", report.collectors);
    assert!(!s.has("snapshot/docker") && !s.has("snapshot/incus") && !s.has("snapshot/monitoring"));
}

#[test]
fn portainer_stacks_are_mapped_to_their_host_path() {
    let s = server("snap-docker-portainer");
    s.host.create_dir_all(Path::new("/opt/yams/config/portainer/compose/3"), 0o755).unwrap();
    s.write(
        "/opt/yams/config/portainer/compose/3/docker-compose.yml",
        "services:\n  whoami:\n    image: traefik/whoami\n",
    );
    change_container(&s, "seerr", |c| {
        let labels = &mut c["Config"]["Labels"];
        labels["com.docker.compose.project"] = serde_json::json!("whoami");
        labels["com.docker.compose.project.working_dir"] = serde_json::json!("/data/compose/3");
        labels["com.docker.compose.project.config_files"] = serde_json::json!("/data/compose/3/docker-compose.yml");
    });
    s.snapshot();
    let projects = json(&s, "snapshot/docker/compose-projects.json");
    let whoami = projects.as_array().unwrap().iter().find(|p| p["project"] == "whoami").unwrap();
    assert_eq!(whoami["workdir"], "/opt/yams/config/portainer/compose/3");
    assert_eq!(whoami["config_files"], serde_json::json!(["/opt/yams/config/portainer/compose/3/docker-compose.yml"]));
    assert_eq!(whoami["defined_services"], serde_json::json!(["whoami"]));
}

// ------------------------------------------------------------------ incus, monitoring

#[test]
fn incus_instances_without_runtime_keys() {
    let s = server("snap-incus");
    s.snapshot();
    assert!(s.read("snapshot/incus/preseed.yaml").contains("driver: btrfs"));
    let devbox = s.read("snapshot/incus/instances/devbox.yaml");
    assert!(devbox.contains("limits.memory: 6GiB") && !devbox.contains("volatile."), "{devbox}");
    assert_eq!(s.read("snapshot/incus/instances/devbox.snapshots"), "base\nbefore-upgrade\nsnap0\n");
    assert!(s.read("volatile/incus-list.txt").contains("RUNNING"));

    // installed but not answering: keep what we had
    s.fixture.add_command("incus list -c n -f csv", 1, "");
    let report = s.snapshot();
    assert_eq!(report.failed().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["incus"]);
    assert!(s.has("snapshot/incus/instances/devbox.yaml"));
}

#[test]
fn integrations_list_everything_that_pushes_and_who_runs_it() {
    let s = server("snap-integrations");
    s.snapshot();
    let found = json(&s, "snapshot/monitoring/integrations.json");
    let found = found.as_array().unwrap();
    let file = |path: &str| {
        found.iter().find(|i| i["file"] == path).unwrap_or_else(|| panic!("{path} not found in {found:#?}"))
    };

    let resources = file("/opt/scripts/resource-check.sh");
    assert_eq!(
        (resources["kind"].as_str(), resources["base"].as_str(), resources["token"].as_str()),
        (Some("uptime-kuma-push"), Some("http://uptime.home"), Some("FAKEresourcesTok01"))
    );
    assert_eq!(
        resources["called_by"],
        serde_json::json!([
            "/etc/cron.d/resource-check: */5 * * * * root flock -n /run/resource-check.lock /opt/scripts/resource-check.sh 2>&1 | systemd-cat -t resource-check"
        ])
    );
    assert_eq!(
        file("/opt/scripts/backup.sh")["called_by"],
        serde_json::json!(["/etc/systemd/system/server-backup.service: ExecStart=/opt/scripts/backup.sh"])
    );
    // run from cron and from the post-boot self-check unit
    assert_eq!(file("/opt/scripts/service-check.sh")["called_by"].as_array().unwrap().len(), 2);

    // a forgotten copy still holding a token is called out; nothing runs it
    let stale = file("/opt/scripts/backup.sh.bak");
    assert_eq!((stale["stale_copy"].as_bool(), stale["token"].as_str()), (Some(true), Some("FAKEoldBackupTok")));
    assert_eq!(stale["called_by"], serde_json::json!([]));

    // a container that pushes by itself
    let ddns = found.iter().find(|i| i["container"] == "ddns").unwrap();
    assert_eq!((ddns["env"].as_str(), ddns["token"].as_str()), (Some("UPTIMEKUMA"), Some("FAKEddnsPush01")));
}

#[test]
fn kuma_monitors_are_read_without_touching_kumas_database() {
    let s = server("snap-kuma");
    let data_dir = common::fixture_dir("cestmoi").join("root/opt/services/monitoring/uptime-kuma/data");
    let before = std::fs::read(data_dir.join("kuma.db")).unwrap();
    s.snapshot();
    assert_eq!(std::fs::read(data_dir.join("kuma.db")).unwrap(), before);
    assert_eq!(
        std::fs::read_dir(&data_dir).unwrap().count(),
        1,
        "no journal, WAL or lock file may appear next to kuma.db"
    );

    let kuma = json(&s, "snapshot/monitoring/kuma-monitors.json");
    assert_eq!(kuma["db"], "/opt/services/monitoring/uptime-kuma/data/kuma.db");
    let monitors = kuma["monitors"].as_array().unwrap();
    assert_eq!(monitors.len(), 21);
    let monitor = |name: &str| monitors.iter().find(|m| m["name"] == name).unwrap();
    let backup = monitor("Server Backup");
    assert_eq!(
        (backup["type"].as_str(), backup["push_token"].as_str(), backup["interval"].as_i64()),
        (Some("push"), Some("FAKEbackupTok00004"), Some(90_000))
    );
    assert_eq!(backup["notifications"], serde_json::json!(["Email"]));
    assert_eq!(monitor("devbox")["active"], 0, "paused monitors say so");
    assert_eq!(monitor("Notes")["tags"], serde_json::json!(["public"]));
    assert_eq!(
        monitor("healthchecks.io watchdog")["url"],
        "https://hc-ping.com/FAKEpingkey0000000000/cestmoi-watchdog"
    );
    assert!(
        !s.read("snapshot/monitoring/kuma-monitors.json").contains("FAKEsmtppassword"),
        "notification settings are not copied"
    );

    // what monitors last said changes every minute: volatile
    let last = json(&s, "volatile/kuma-last.json");
    assert_eq!(
        last["Server Backup"],
        serde_json::json!({ "msg": "[object Object]", "status": "up", "time": "2026-11-26 16:55:00.000" })
    );
    assert!(last.get("devbox").is_none());
    assert!(!s.read("snapshot/monitoring/kuma-monitors.json").contains("object Object"));
}

// ------------------------------------------------------------------ golden

/// The whole stable tree of the `cestmoi` fixture, except the verbatim copies under `files/` (the
/// manifest pins those by hash). Regenerate with `MOI_UPDATE_GOLDEN=1 cargo test --test snapshot golden`
/// and review the diff.
#[test]
fn golden_snapshot_of_the_cestmoi_fixture() {
    let s = server("snap-golden");
    let report = s.snapshot();
    assert_eq!(report.failed().count(), 0, "{:?}", report.collectors);
    let expected_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/cestmoi/snapshot");
    let actual: Vec<String> = snapshot::list_tree(s.host.as_ref(), &s.state.state(), "snapshot")
        .into_iter()
        .filter_map(|p| p.strip_prefix("snapshot/").map(str::to_string))
        .filter(|p| !p.starts_with("files/"))
        .collect();

    if std::env::var_os("MOI_UPDATE_GOLDEN").is_some() {
        let _ = std::fs::remove_dir_all(&expected_dir);
    }
    for rel in &actual {
        common::assert_golden(&expected_dir.join(rel), &s.read(&format!("snapshot/{rel}")));
    }
    // …and nothing in the golden tree that the snapshot no longer produces
    let mut golden = Vec::new();
    let mut stack = vec![expected_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                golden.push(path.strip_prefix(&expected_dir).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    golden.sort();
    assert_eq!(golden, actual, "the set of snapshot files differs from the golden tree");
}

#[test]
fn the_raw_files_of_a_database_are_never_captured_whatever_its_directory_is_called() {
    // bookorbit-db keeps /var/lib/mysql in a directory of its stack that no prune name matches
    let s = server("snap-raw-database");
    s.host.create_dir_all(Path::new("/opt/yams/config/bookorbit-db/mysql"), 0o755).unwrap();
    s.write("/opt/yams/config/bookorbit-db/mariadb_upgrade_info", "11.8.2-MariaDB\n");
    s.write("/opt/yams/config/bookorbit-db/mysql/db.opt", "default-character-set=utf8mb4\n");
    // … next to a file of the stack that is configuration
    s.write("/opt/yams/config/notes.conf", "kept = true\n");
    s.snapshot();
    assert!(s.has("snapshot/files/opt/yams/config/notes.conf"));
    assert!(!s.has("snapshot/files/opt/yams/config/bookorbit-db"));
    assert_eq!(s.manifest_line("/opt/yams/config/bookorbit-db/mariadb_upgrade_info"), None);
}
