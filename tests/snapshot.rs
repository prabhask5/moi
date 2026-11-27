//! Snapshot engine and collectors (docs/spec/02-snapshot.md) on the `cestmoi` fixture host.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cestmoi::config::{self, Config, Paths};
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
    assert_eq!(s.host.read_dir(Path::new("/var/lib")).unwrap(), ["apt", "dpkg", "incus", "moi"]);
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
    assert_eq!(lines(&s.read("volatile/apt-all.tsv")).len(), 10);
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
