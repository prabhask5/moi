//! `moi restore`: the phases, the dry run, and the questions that are never answered for the owner
//! (docs/spec/09-restore-verify.md § Restore).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use cestmoi::cli;
use cestmoi::host::{FileKind, FixtureHost, Host};
use cestmoi::restore::{self, Options};

const BUNDLE: &str = "/root/moi-bundle";

/// Copy a tree from one host to a path on another, through the hosts.
fn copy_tree(from: &dyn Host, source: &Path, to: &dyn Host, target: &Path) {
    match from.symlink_metadata(source).unwrap().kind {
        FileKind::Dir => {
            to.create_dir_all(target, 0o755).unwrap();
            for name in from.read_dir(source).unwrap() {
                copy_tree(from, &source.join(&name), to, &target.join(&name));
            }
        }
        FileKind::Symlink => to.symlink(&from.read_link(source).unwrap(), target).unwrap(),
        _ => to.write(target, &from.read(source).unwrap(), 0o600).unwrap(),
    }
}

/// Put the bundle of the cestmoi server (its state after a snapshot, and its `/etc/moi`) on `machine`.
fn put_bundle(test: &str, machine: &dyn Host) {
    let old: Arc<dyn Host> = Arc::new(
        FixtureHost::open(&common::fixture_dir("cestmoi"))
            .unwrap()
            .with_overlay(&common::scratch(&format!("{test}-old")))
            .unwrap(),
    );
    let mut out = Vec::new();
    cli::execute(&cli::parse(&["snapshot", "--quiet"]).unwrap(), old.clone(), &mut out).unwrap();
    let bundle = Path::new(BUNDLE);
    copy_tree(old.as_ref(), Path::new("/var/lib/moi/state"), machine, &bundle.join("var/lib/moi/state"));
    copy_tree(old.as_ref(), Path::new("/etc/moi"), machine, &bundle.join("etc/moi"));
}

/// A freshly installed machine (the `minimal` fixture, plus what an installer leaves) with the bundle on it.
fn new_machine(test: &str) -> Arc<FixtureHost> {
    let fixture = common::minimal_rw(test);
    let write = |path: &str, text: &str| {
        fixture.create_dir_all(Path::new(path).parent().unwrap(), 0o755).unwrap();
        fixture.write(Path::new(path), text.as_bytes(), 0o644).unwrap();
    };
    write("/etc/os-release", "PRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\nVERSION_CODENAME=noble\n");
    write("/etc/passwd", "root:x:0:0:root:/root:/bin/bash\nubuntu:x:1001:1001:Ubuntu:/home/ubuntu:/bin/bash\n");
    write("/etc/group", "root:x:0:\nsudo:x:27:ubuntu\nubuntu:x:1001:\n");
    write("/etc/fstab", "/dev/vda2 / ext4 defaults 0 1\n");
    for tool in ["apt-get", "snap", "systemctl"] {
        write(&format!("/usr/bin/{tool}"), "");
    }
    fixture.add_command("uname -m", 0, "x86_64\n");
    fixture.add_command(
        "lsblk -o NAME,SIZE,TYPE,FSTYPE,LABEL,MODEL,MOUNTPOINT -e7",
        0,
        "NAME   SIZE TYPE FSTYPE LABEL MODEL       MOUNTPOINT\nvda    100G disk              QEMU HARDDISK\n└─vda2 100G part ext4               /\nsdb    3.6T disk              WDC WD40\n",
    );
    put_bundle(test, fixture.as_ref());
    fixture
}

fn run(host: &Arc<FixtureHost>, options: &Options) -> (usize, String) {
    let host: Arc<dyn Host> = host.clone();
    let mut out = Vec::new();
    let warnings = restore::restore(&host, Path::new(BUNDLE), options, &mut out).unwrap();
    (warnings, String::from_utf8(out).unwrap())
}

fn only(phases: &[&str]) -> Options {
    Options { only: phases.iter().map(|p| (*p).to_string()).collect(), yes: true, ..Options::default() }
}

fn fstab(host: &FixtureHost) -> String {
    host.read_to_string(Path::new("/etc/fstab")).unwrap()
}

#[test]
fn list_shows_every_phase_in_order() {
    let host: Arc<dyn Host> = Arc::new(common::minimal());
    let mut out = Vec::new();
    cli::execute(&cli::parse(&["restore", "--list"]).unwrap(), host, &mut out).unwrap();
    let names: Vec<String> =
        String::from_utf8(out).unwrap().lines().map(|l| l.split_whitespace().next().unwrap().to_string()).collect();
    assert_eq!(
        names,
        [
            "preflight",
            "packages",
            "users",
            "home",
            "storage",
            "dirs",
            "docker-engine",
            "data",
            "config",
            "ssh",
            "scripts",
            "incus",
            "docker",
            "moi",
            "systemd",
            "cron",
            "firewall",
            "monitors",
            "verify"
        ]
    );
}

#[test]
fn only_from_and_skip_select_phases() {
    let phases = |only: &[&str], from: Option<&str>, skip: &[&str]| {
        restore::selected(&Options {
            only: only.iter().map(|s| (*s).to_string()).collect(),
            from: from.map(str::to_string),
            skip: skip.iter().map(|s| (*s).to_string()).collect(),
            ..Options::default()
        })
    };
    assert_eq!(phases(&[], None, &[]).unwrap().len(), 19);
    assert_eq!(phases(&["cron", "users"], None, &[]).unwrap(), ["users", "cron"]);
    assert_eq!(phases(&[], Some("firewall"), &["monitors"]).unwrap(), ["firewall", "verify"]);
    let unknown = phases(&[], Some("disks"), &[]).unwrap_err().to_string();
    assert!(unknown.contains("no such phase: disks"), "{unknown}");
}

#[test]
fn a_directory_that_is_not_a_bundle_is_refused() {
    let host: Arc<dyn Host> = common::minimal_rw("restore-not-a-bundle");
    let error =
        restore::restore(&host, Path::new("/etc"), &Options::default(), &mut Vec::new()).unwrap_err().to_string();
    assert!(error.contains("does not look like a state bundle"), "{error}");
}

#[test]
fn full_dry_run_on_a_new_machine_changes_nothing() {
    let host = new_machine("restore-dry-run");
    let before = host.commands_run().len();
    let (_, out) = run(&host, &Options { dry_run: true, ..Options::default() });
    common::assert_golden(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/cestmoi/restore-dry-run.txt"),
        &out,
    );
    // every phase is announced, in order
    let announced: Vec<&str> =
        out.lines().filter_map(|l| l.strip_prefix("==> phase: ")).map(|l| l.split(' ').next().unwrap()).collect();
    assert_eq!(announced.len(), 19);
    // only questions were asked of the machine
    let ran = host.commands_run()[before..].to_vec();
    for line in &ran {
        let reads = ["uname ", "apt-cache ", "lsblk ", "blkid ", "restic snapshots "];
        assert!(reads.iter().any(|r| line.starts_with(r)), "a dry run ran `{line}`");
    }
    assert!(host.asked().is_empty(), "a dry run asked: {:?}", host.asked());
    assert_eq!(fstab(&host), "/dev/vda2 / ext4 defaults 0 1\n");
    assert!(!host.exists(Path::new("/var/lib/moi/restore/restore.log")));
    assert!(!host.exists(Path::new("/etc/ssh/sshd_config.d")));
}

#[test]
fn a_disk_that_was_moved_over_is_recognized_by_its_uuid() {
    let host = new_machine("restore-moved-disk");
    host.add_command("blkid -U aaaaaaaa-0000-4000-8000-000000000001", 0, "/dev/sdc1\n");
    for command in ["mkdir -p /mnt/storage", "systemctl daemon-reload", "mount -a"] {
        host.add_command(command, 0, "");
    }
    let (warnings, out) = run(&host, &only(&["storage"]));
    assert_eq!(warnings, 0, "{out}");
    assert!(out.contains("the SAME filesystem is present here (/dev/sdc1)"), "{out}");
    // mounted where it was, by the name the old fstab used; nobody was asked for a device
    assert_eq!(fstab(&host), "/dev/vda2 / ext4 defaults 0 1\nLABEL=storage /mnt/storage ext4 defaults,nofail 0 2\n");
    assert!(host.asked().is_empty(), "{:?}", host.asked());
    assert!(!host.commands_run().iter().any(|c| c.starts_with("mkfs")));
    // the fstab that was there is kept
    let kept: Vec<String> =
        host.read_dir(Path::new("/etc")).unwrap().into_iter().filter(|n| n.starts_with("fstab.pre-moi.")).collect();
    assert_eq!(kept.len(), 1);
    assert!(host.read_to_string(Path::new("/var/lib/moi/restore/restore.log")).unwrap().contains("==> phase: storage"));
}

fn empty_disk(test: &str) -> Arc<FixtureHost> {
    let host = new_machine(test);
    host.create_dir_all(Path::new("/dev"), 0o755).unwrap();
    host.write(Path::new("/dev/sdb1"), b"", 0o600).unwrap();
    host.add_command("blkid -U aaaaaaaa-0000-4000-8000-000000000001", 2, "");
    host.add_command_then("blkid -o value -s TYPE /dev/sdb1", 0, "");
    host.add_command("blkid -o value -s UUID /dev/sdb1", 0, "bbbbbbbb-0000-4000-8000-000000000002\n");
    for command in ["mkfs -t ext4 -L storage /dev/sdb1", "mkdir -p /mnt/storage", "systemctl daemon-reload", "mount -a"]
    {
        host.add_command(command, 0, "");
    }
    host
}

#[test]
fn a_disk_is_formatted_only_for_someone_who_types_format_and_its_name() {
    // `--yes` is on in both runs: it answers neither question
    let host = empty_disk("restore-format-refused");
    host.answer("/dev/sdb1");
    host.answer("yes");
    let (_, out) = run(&host, &only(&["storage"]));
    assert!(
        host.asked()
            .iter()
            .any(|q| q.contains("Type  FORMAT /dev/sdb1  to create ext4 (label storage) — THIS ERASES IT")),
        "{:?}",
        host.asked()
    );
    assert!(!host.commands_run().iter().any(|c| c.starts_with("mkfs")), "{:?}", host.commands_run());
    assert!(out.contains("skipped /mnt/storage"), "{out}");
    assert_eq!(fstab(&host), "/dev/vda2 / ext4 defaults 0 1\n");

    let host = empty_disk("restore-format-typed");
    host.answer("/dev/sdb1");
    host.answer("FORMAT /dev/sdb1");
    let (warnings, out) = run(&host, &only(&["storage"]));
    assert_eq!(warnings, 0, "{out}");
    assert!(host.commands_run().iter().any(|c| c == "mkfs -t ext4 -L storage /dev/sdb1"), "{:?}", host.commands_run());
    assert_eq!(
        fstab(&host),
        "/dev/vda2 / ext4 defaults 0 1\nUUID=bbbbbbbb-0000-4000-8000-000000000002 /mnt/storage ext4 defaults,nofail 0 2\n"
    );
}

#[test]
fn nobody_at_the_terminal_means_no_device_and_no_format() {
    let host = empty_disk("restore-format-nobody");
    let (_, out) = run(&host, &only(&["storage"]));
    assert!(out.contains("skipped /mnt/storage"), "{out}");
    assert!(!host.commands_run().iter().any(|c| c.starts_with("mkfs")));
}

#[test]
fn restored_files_keep_what_they_replace() {
    let host = new_machine("restore-pre-moi");
    host.create_dir_all(Path::new("/etc/ssh/sshd_config.d"), 0o755).unwrap();
    let existing =
        host.read_dir(&Path::new(BUNDLE).join("var/lib/moi/state/snapshot/files/etc/ssh/sshd_config.d")).unwrap();
    let name = existing.first().expect("the cestmoi snapshot keeps an sshd drop-in");
    let target = Path::new("/etc/ssh/sshd_config.d").join(name);
    host.write(&target, b"PasswordAuthentication yes\n", 0o600).unwrap();
    let (_, out) = run(&host, &only(&["config"]));
    assert!(out.contains("files under /etc put back"), "{out}");
    let kept = host.read_to_string(Path::new(&format!("{}.pre-moi", target.display()))).unwrap();
    assert_eq!(kept, "PasswordAuthentication yes\n");
    assert_ne!(host.read_to_string(&target).unwrap(), kept);
    // never copied between machines
    assert_eq!(host.read_to_string(Path::new("/etc/fstab")).unwrap(), "/dev/vda2 / ext4 defaults 0 1\n");
    assert!(
        out.contains("the network configuration is NOT applied")
            || !host.exists(&Path::new(BUNDLE).join("var/lib/moi/state/snapshot/files/etc/netplan"))
    );
    assert!(!host.commands_run().iter().any(|c| c.starts_with("netplan")));
}

/// The cestmoi machine itself as the target (Docker, firewall tools and Kuma answer there), with its
/// own bundle on it.
fn same_machine(test: &str, case: Option<&Path>) -> Arc<FixtureHost> {
    let mut fixture = FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap();
    if let Some(case) = case {
        fixture = fixture.with_case(case).unwrap();
    }
    let fixture = Arc::new(fixture.with_overlay(&common::scratch(test)).unwrap());
    put_bundle(test, fixture.as_ref());
    fixture
}

#[test]
fn the_data_comes_back_from_the_latest_snapshot_of_the_old_servers_repository() {
    let host = new_machine("restore-data");
    host.add_command(
        "restic snapshots --tag nightly --json",
        0,
        r#"[{"time":"2026-11-24T03:31:00Z","id":"1111aaaa","short_id":"1111"},{"time":"2026-11-25T03:31:00Z","id":"2222bbbb","short_id":"2222"}]"#,
    );
    host.add_command("restic restore 2222bbbb --target /", 0, "");
    host.add_command("mkdir -p /mnt/storage/media", 0, "");
    host.add_command("mkdir -p /var/lib/incus", 0, "");
    let (warnings, out) = run(&host, &only(&["data"]));
    assert_eq!(warnings, 0, "{out}");
    assert!(out.contains("latest `nightly` snapshot: 2222 from 2026-11-25T03:31:00Z"), "{out}");
    assert!(
        out.contains("not in the backup: /mnt/storage/media — media library — temporary, re-downloadable"),
        "{out}"
    );
    let ran = host.commands_run();
    assert!(ran.iter().any(|c| c == "restic restore 2222bbbb --target /"), "{ran:?}");
    // what is never backed up gets its place back, empty
    assert!(ran.iter().any(|c| c == "mkdir -p /mnt/storage/media"), "{ran:?}");
    // the repository and its password come from the bundle's secrets, in the environment only
    let env = host.environment_of("restic restore");
    assert!(env.iter().any(|(k, v)| k == "RESTIC_REPOSITORY" && v == "FAKE-restic_repository-value"), "{env:?}");
    assert!(env.iter().any(|(k, _)| k == "RESTIC_PASSWORD"), "{env:?}");
    assert!(!ran.iter().any(|c| c.contains("FAKE")), "a secret on a command line: {ran:?}");

    // a named snapshot instead
    let host = new_machine("restore-data-named");
    host.add_command("restic restore 1111aaaa --target /", 0, "");
    let options = Options { snapshot: Some("1111aaaa".to_string()), ..only(&["data"]) };
    run(&host, &options);
    assert!(host.commands_run().iter().any(|c| c == "restic restore 1111aaaa --target /"));
    assert!(!host.commands_run().iter().any(|c| c.starts_with("restic snapshots")));
}

#[test]
fn databases_come_up_first_and_an_empty_one_is_loaded_from_its_dump() {
    // outline's database has an empty data directory (a new disk); the others kept theirs
    let case = common::scratch("restore-db-first-case");
    std::fs::write(case.join("absent"), "/opt/services/outline/pgdata\n").unwrap();
    let exec = case.join("docker/exec/outline-postgres");
    std::fs::create_dir_all(&exec).unwrap();
    for line in ["pg_isready -q -U outline", "psql -q -U outline -d postgres -f -"] {
        std::fs::write(exec.join(format!("{}.out", FixtureHost::command_key(line))), "").unwrap();
    }
    let host = same_machine("restore-db-first", Some(&case));
    host.create_dir_all(Path::new("/opt/services/outline/pgdata"), 0o700).unwrap();
    let dump = "CREATE ROLE outline;\nCREATE DATABASE outline;\n";
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, dump.as_bytes()).unwrap();
    host.create_dir_all(Path::new("/mnt/storage/backups/2026-11-26_0330/db"), 0o700).unwrap();
    host.write(
        Path::new("/mnt/storage/backups/2026-11-26_0330/db/outline-postgres.sql.gz"),
        &gz.finish().unwrap(),
        0o600,
    )
    .unwrap();
    for kept in ["/mnt/storage/database/db-data", "/opt/yams/config/bookorbit-db"] {
        host.create_dir_all(Path::new(kept), 0o700).unwrap();
        host.write(&Path::new(kept).join("PG_VERSION"), b"18\n", 0o600).unwrap();
    }
    host.add_command_then("docker compose ...", 0, "");

    let (_, out) = run(&host, &only(&["docker"]));
    let compose: Vec<String> =
        host.commands_run().into_iter().filter(|c| c.starts_with("docker compose -p outline ")).collect();
    let ends: Vec<&str> = compose.iter().map(|c| c.rsplit(".env ").next().unwrap()).collect();
    assert_eq!(ends, ["pull -q --ignore-pull-failures", "up -d postgres", "up -d"], "{compose:?}");
    // the dump went into the database's standard input, between the two `up`s
    assert!(
        out.contains("outline-postgres: dump loaded (/mnt/storage/backups/2026-11-26_0330/db/outline-postgres.sql.gz)"),
        "{out}"
    );
    let sent = host.docker_sent();
    let loaded: Vec<&(String, Vec<u8>)> = sent
        .iter()
        .filter(|(line, _)| line.starts_with("POST /exec/") && line.ends_with("-outline-postgres/start"))
        .collect();
    assert!(
        loaded.iter().any(|(_, input)| input == dump.as_bytes()),
        "{:?}",
        sent.iter().map(|(l, _)| l).collect::<Vec<_>>()
    );
    let psql = sent
        .iter()
        .find(|(line, body)| {
            line == "POST /containers/outline-postgres/exec" && String::from_utf8_lossy(body).contains("psql")
        })
        .unwrap();
    let spec: serde_json::Value = serde_json::from_slice(&psql.1).unwrap();
    assert_eq!(spec["AttachStdin"], true);
    assert_eq!(spec["Env"], serde_json::json!(["PGPASSWORD=FAKEoutlinedbpass"]));
    // a database whose data is there is left alone
    assert!(out.contains("supabase-db: its data directory has data"), "{out}");
    assert!(!sent.iter().any(|(line, _)| line == "POST /containers/supabase-db/exec"));
    // what already exists on this machine is not created again
    let requests = host.docker_requests();
    assert!(
        !requests.iter().any(|r| r.starts_with("POST /networks/create")
            || r.starts_with("POST /services/create")
            || r.starts_with("POST /swarm/init")),
        "{requests:?}"
    );
    // a stack that was stopped on the old server stays stopped
    assert!(out.contains("stack old-wiki was not running on the old server"), "{out}");
    assert!(!host.commands_run().iter().any(|c| c.starts_with("docker compose -p old-wiki ")));
}

const SAVED_V4: &str = "/var/lib/moi/archive/firewall/2026-11-26T170000Z.v4.rules";
const SAVED_V6: &str = "/var/lib/moi/archive/firewall/2026-11-26T170000Z.v6.rules";

fn over_ssh(test: &str) -> Arc<FixtureHost> {
    let host = same_machine(test, None);
    host.set_env("SSH_CONNECTION", "203.0.113.5 50000 192.0.2.10 22");
    host.add_command(
        &format!("systemd-run --quiet --collect --unit=moi-firewall-rollback --on-active=90s /bin/sh -c iptables-restore '{SAVED_V4}'; ip6tables-restore '{SAVED_V6}'"),
        0,
        "",
    );
    host.add_command("systemctl stop moi-firewall-rollback.timer", 0, "");
    host.add_command(&format!("iptables-restore {SAVED_V4}"), 0, "");
    host.add_command(&format!("ip6tables-restore {SAVED_V6}"), 0, "");
    host
}

fn firewall_commands(host: &FixtureHost) -> Vec<String> {
    host.commands_run().into_iter().filter(|c| c.contains("tables-restore") || c.starts_with("systemd-run")).collect()
}

#[test]
fn the_firewall_is_never_applied_on_yes_alone() {
    // `--yes` is on; nobody types the confirmation
    let host = over_ssh("restore-firewall-unasked");
    let (_, out) = run(&host, &only(&["firewall"]));
    assert!(out.contains("not applied: `moi firewall apply` when you are ready"), "{out}");
    assert!(firewall_commands(&host).is_empty(), "{:?}", firewall_commands(&host));
    assert_eq!(host.asked().len(), 1);
}

#[test]
fn a_firewall_that_is_not_confirmed_over_ssh_is_rolled_back() {
    let host = over_ssh("restore-firewall-rollback");
    host.answer("yes"); // apply it — and then the session is cut off: no second answer
    let (warnings, out) = run(&host, &only(&["firewall"]));
    assert_eq!(warnings, 1, "{out}");
    assert!(
        out.contains("WARN: the firewall was not applied: not confirmed: the previous rules are back in place"),
        "{out}"
    );
    let commands = firewall_commands(&host);
    let position = |needle: &str| {
        commands.iter().position(|c| c.starts_with(needle)).unwrap_or_else(|| panic!("{needle} not run: {commands:?}"))
    };
    assert!(
        position("systemd-run") < position("iptables-restore --noflush"),
        "the way back is arranged first: {commands:?}"
    );
    assert!(position("iptables-restore --noflush") < position(&format!("iptables-restore {SAVED_V4}")), "{commands:?}");
    assert!(commands.contains(&format!("ip6tables-restore {SAVED_V6}")), "{commands:?}");

    // confirmed: kept
    let host = over_ssh("restore-firewall-kept");
    host.answer("yes");
    host.answer("yes");
    let (warnings, out) = run(&host, &only(&["firewall"]));
    assert_eq!(warnings, 0, "{out}");
    assert!(out.contains("firewall applied: "), "{out}");
    assert!(!firewall_commands(&host).contains(&format!("iptables-restore {SAVED_V4}")));
}

#[test]
fn every_phase_asks_first_and_a_declined_one_is_skipped() {
    let host = new_machine("restore-declined");
    host.answer("n");
    let options = Options { only: vec!["users".to_string()], ..Options::default() };
    let (warnings, out) = run(&host, &options);
    assert_eq!(warnings, 0);
    assert_eq!(host.asked(), ["  Run phase 'users'? [Y/n] "]);
    assert!(out.contains("    skipped"), "{out}");
    assert!(!host.commands_run().iter().any(|c| c.starts_with("useradd") || c.starts_with("groupadd")));
}

#[test]
fn users_come_back_with_their_ids_and_the_password_hash_stays_off_the_command_line() {
    let host = new_machine("restore-users");
    for command in [
        "groupadd -g 1000 prabhas",
        "useradd -u 1000 -g 1000 -d /home/prabhas -m -s /bin/bash -c Prabhas prabhas",
        "chpasswd -e",
        "usermod -aG sudo prabhas",
        "groupadd -r docker",
        "usermod -aG docker prabhas",
        "groupadd -r incus-admin",
        "usermod -aG incus-admin prabhas",
    ] {
        host.add_command(command, 0, "");
    }
    let (warnings, out) = run(&host, &only(&["users"]));
    assert_eq!(warnings, 0, "{out}");
    let ran: Vec<String> = host
        .commands_run()
        .into_iter()
        .filter(|c| c.contains("add") || c.contains("mod") || c.contains("passwd"))
        .collect();
    assert_eq!(ran[0], "groupadd -g 1000 prabhas");
    assert_eq!(ran[1], "useradd -u 1000 -g 1000 -d /home/prabhas -m -s /bin/bash -c Prabhas prabhas");
    assert!(
        ran.contains(&"groupadd -r docker".to_string()) && ran.contains(&"usermod -aG docker prabhas".to_string()),
        "{ran:?}"
    );
    // root is never recreated; the hash goes in on standard input
    assert!(!ran.iter().any(|c| c.contains(" root")), "{ran:?}");
    let inputs = host.inputs();
    let (_, input) = inputs.iter().find(|(command, _)| command == "chpasswd -e").expect("chpasswd got input");
    assert!(input.starts_with("prabhas:$"), "{input}");
    let log = host.read_to_string(Path::new("/var/lib/moi/restore/restore.log")).unwrap();
    assert!(!log.contains(input.trim().split(':').nth(1).unwrap()), "the hash is in the log");
}

#[test]
fn a_service_with_docker_secrets_is_named_not_recreated() {
    // Docker never gives a secret's value back: such a service cannot be rebuilt from what was captured
    let host = new_machine("restore-swarm-secrets");
    let file = Path::new(BUNDLE).join("var/lib/moi/state/snapshot/docker/services.json");
    let mut services: serde_json::Value = serde_json::from_slice(&host.read(&file).unwrap()).unwrap();
    for service in services.as_array_mut().unwrap() {
        if service["name"] == "dokploy-postgres" {
            service["secrets"] = serde_json::json!(["dokploy_postgres_password"]);
        }
    }
    host.write(&file, services.to_string().as_bytes(), 0o600).unwrap();
    let (_, out) = run(&host, &Options { dry_run: true, only: vec!["docker".to_string()], ..Options::default() });
    assert!(
        out.contains("WARN: service dokploy-postgres uses Docker secrets or configs (dokploy_postgres_password) whose values Docker does not give back"),
        "{out}"
    );
    assert!(!out.contains("create service dokploy-postgres"), "{out}");
    assert!(out.contains("create service dokploy (dokploy/dokploy:v0.31.0)"), "{out}");
}

#[test]
fn the_directory_tree_stops_at_a_database_data_directory() {
    // a database initializes only an empty directory: its subdirectories are its own to create
    let host = new_machine("restore-dirs-db");
    let trees = Path::new(BUNDLE).join("var/lib/moi/state/volatile/trees");
    let file = host.read_dir(&trees).unwrap().into_iter().find(|n| n.contains("opt")).expect("a tree of /opt");
    let mut tree = host.read_to_string(&trees.join(&file)).unwrap();
    tree.push_str(
        "0700\troot\troot\t/opt/services/outline/pgdata\n0700\troot\troot\t/opt/services/outline/pgdata/base\n",
    );
    host.write(&trees.join(&file), tree.as_bytes(), 0o600).unwrap();
    host.create_dir_all(Path::new("/opt"), 0o755).unwrap();
    let (_, out) = run(&host, &only(&["dirs"]));
    assert!(host.exists(Path::new("/opt/services/outline/pgdata")), "{out}");
    assert!(!host.exists(Path::new("/opt/services/outline/pgdata/base")), "{out}");
}
