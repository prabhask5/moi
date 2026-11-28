//! `moi devbox`: devbox's host commands through moi's own host, firewall and records
//! (docs/spec/15-devbox.md). The machine's Incus and the programs devbox runs are devbox's own
//! fakes (`devbox_core::fake`), put behind a moi fixture host; what is printed is compared with
//! devbox's golden files, copied from its `crates-v0.1.0` tag into `tests/fixtures/expected/devbox/`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use cestmoi::cli;
use cestmoi::devbox::{self, DevboxHost};
use cestmoi::host::{Cmd, CmdOutput, Conn, FixtureHost, Host};
use devbox_core::fake::FakeSystem;
use devbox_core::system::{self as dsys, System};

const HEALTH_OK: &str =
    r#"{"v":1,"status":"ok","problems":[],"vm":"running","phase":"idle","checkedAt":1790000000000}"#;
const GATEWAY_STATUS: &str = r#"{"v":1,"version":"3f9c2d1","phase":"idle","vm":"running","lastStop":{"at":1789990000000,"reason":"idle","mode":"stateful","ms":4200}}"#;
const VM_FINE: &str = "code_server=active\nhub=active\ntmux=active\ncontext_timer=active\nupdate_timer=active\nagent_claude=yes\nagent_codex=yes\nagent_opencode=yes\nhooks_claude=yes\nhooks_codex=yes\nhooks_opencode=yes\nsync=2026-09-21T14:00:00Z\ncontext=2026-09-21T12:00:00Z\ndotfiles=0 0\n";
const INCUS: &str = "/var/lib/incus/unix.socket";

fn golden(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/devbox").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn mine(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/devbox/moi").join(name)
}

/// devbox's connection to its fake Incus, as a moi connection.
#[derive(Debug)]
struct Bridge(Box<dyn dsys::Conn>);

impl Read for Bridge {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for Bridge {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.write(data)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Conn for Bridge {
    fn set_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.0.set_timeout(timeout)
    }
}

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
    /// devbox's fake machine: only its programs and its Incus are used; the files are the fixture's.
    machine: Arc<FakeSystem>,
}

fn printed(text: &str) -> Option<dsys::Output> {
    Some(dsys::Output { code: Some(0), stdout: text.as_bytes().to_vec(), ..dsys::Output::default() })
}

/// The cestmoi server with what devbox's own tests put on theirs (`tests/rust/src/hosts.rs` there):
/// Incus, the gateway deployed, `dev` running the command with sudo.
fn server(test: &str) -> Server {
    let fixture = Arc::new(
        FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&common::scratch(test)).unwrap(),
    );
    let machine = Arc::new(FakeSystem::new());
    machine.prints("id", "0\n");
    machine.program("docker", |command| match command.args.first().map(String::as_str) {
        Some("inspect") => printed("running\n"),
        Some("compose") => None,
        _ => printed("29.1.0\n"),
    });
    machine.program("getent", |command| {
        printed(&format!("203.0.113.7 {}\n", command.args.last().map(String::as_str).unwrap_or_default()))
    });
    machine.program("sh", |command| {
        let script = command.args.get(1).map(String::as_str).unwrap_or_default();
        if script.starts_with("getent passwd dev") { printed("/home/dev\n") } else { None }
    });
    machine.program("curl", |command| {
        let url = command.args.last().map(String::as_str).unwrap_or_default();
        if url.ends_with("/__devbox/health") {
            printed(HEALTH_OK)
        } else if url.ends_with("/__devbox/status") {
            printed(GATEWAY_STATUS)
        } else if url.ends_with("/releases/latest") {
            printed(r#"{"tag_name": "v1.2.3"}"#)
        } else {
            None
        }
    });
    machine.incus(|incus| {
        incus.inside = Some(Box::new(|command: &[String]| {
            let script = command.get(2).map(String::as_str).unwrap_or_default();
            if script.starts_with("id -u 'dev'") {
                (0, "1000\n1000\n/home/dev\n".to_owned(), String::new())
            } else if script.contains("echo \"code_server=") {
                (0, VM_FINE.to_owned(), String::new())
            } else {
                (0, "done\n".to_owned(), String::new())
            }
        }));
    });
    // the programs devbox runs are the fake machine's; a command given a directory arrives as `env -C`
    let programs = machine.clone();
    fixture.set_programs(Arc::new(move |cmd: &Cmd| {
        let (program, args, cwd) = match (cmd.program.as_str(), cmd.args.as_slice()) {
            ("env", [flag, dir, program, rest @ ..]) if flag == "-C" => {
                (program.clone(), rest.to_vec(), Some(PathBuf::from(dir)))
            }
            _ => (cmd.program.clone(), cmd.args.clone(), None),
        };
        let command =
            dsys::Command { program, args, cwd, env: cmd.env.clone(), stdin: cmd.stdin.clone(), timeout: cmd.timeout };
        programs.run(&command).ok().map(|out| CmdOutput {
            status: out.code.unwrap_or(1),
            stdout: out.stdout,
            stderr: out.stderr,
            timed_out: out.timed_out,
        })
    }));
    let incus = machine.clone();
    fixture.set_unix(
        Path::new(INCUS),
        Arc::new(move || Ok(Box::new(Bridge(incus.connect_unix(Path::new(INCUS))?)) as Box<dyn Conn>)),
    );
    fixture.set_env("SUDO_USER", "dev");
    fixture.set_now(UNIX_EPOCH + Duration::from_secs(1_790_000_000));
    let s = Server { host: fixture.clone(), fixture, machine };
    s.write("/home/dev/.ssh/authorized_keys", "ssh-ed25519 FAKEkeyFAKEkeyFAKEkey dev@laptop\n# a comment\n", 0o600);
    s.write("/etc/dokploy/traefik/dynamic/dokploy.yml", "http: {}\n", 0o644);
    s.write("/root/wake", "FAKE-wake-password\n", 0o600);
    s.write("/root/app", "FAKE-app-password\n", 0o600);
    // what devbox's own `moi` manager asks when it only looks (doctor): is moi's firewall in force
    s.fixture.add_command("moi firewall status", 0, "");
    s
}

impl Server {
    fn moi(&self, args: &[&str]) -> (u8, String) {
        let mut out = Vec::new();
        match cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out) {
            Ok(()) => (0, String::from_utf8(out).unwrap()),
            Err(e) if e.exit_code() == 1 => (1, String::from_utf8(out).unwrap()),
            Err(e) => (e.exit_code(), format!("{}{e}", String::from_utf8(out).unwrap())),
        }
    }

    fn write(&self, path: &str, text: &str, mode: u32) {
        self.host.create_dir_all(Path::new(path).parent().unwrap(), 0o755).unwrap();
        self.host.write(Path::new(path), text.as_bytes(), mode).unwrap();
    }

    fn read(&self, path: &str) -> String {
        self.host.read_to_string(Path::new(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn up(&self) -> (u8, String) {
        self.moi(&[
            "devbox",
            "up",
            "--domain",
            "example.com",
            "--wake-password-file",
            "/root/wake",
            "--app-password-file",
            "/root/app",
            "--yes",
        ])
    }
}

#[test]
fn every_system_method_goes_through_the_moi_host() {
    let s = server("devbox-adapter");
    let system = DevboxHost(s.host.as_ref());
    // files with their modes and owners; directories made on the way
    system.write(Path::new("/etc/devbox/new/file.toml"), b"a = 1\n", 0o600).unwrap();
    system.append(Path::new("/etc/devbox/new/file.toml"), b"b = 2\n", 0o600).unwrap();
    assert_eq!(system.read(Path::new("/etc/devbox/new/file.toml")).unwrap(), b"a = 1\nb = 2\n");
    let meta = system.metadata(Path::new("/etc/devbox/new/file.toml")).unwrap();
    assert_eq!((meta.kind, meta.mode, meta.size), (dsys::FileKind::File, 0o600, 12));
    system.set_owner(Path::new("/etc/devbox/new/file.toml"), 65532, 65532).unwrap();
    system.set_mode(Path::new("/etc/devbox/new/file.toml"), 0o640).unwrap();
    let meta = s.host.metadata(Path::new("/etc/devbox/new/file.toml")).unwrap();
    assert_eq!((meta.mode, meta.uid, meta.gid), (0o640, 65532, 65532));
    system.create_dir_all(Path::new("/var/lib/devbox/gateway"), 0o750).unwrap();
    assert_eq!(system.metadata(Path::new("/var/lib/devbox/gateway")).unwrap().kind, dsys::FileKind::Dir);
    assert_eq!(system.read_dir(Path::new("/etc/devbox/new")).unwrap(), ["file.toml"]);
    system.symlink(Path::new("file.toml"), Path::new("/etc/devbox/new/link")).unwrap();
    assert_eq!(system.read_link(Path::new("/etc/devbox/new/link")).unwrap(), Path::new("file.toml"));
    assert!(system.exists(Path::new("/etc/devbox/new/file.toml")));
    system.remove_file(Path::new("/etc/devbox/new/file.toml")).unwrap();
    system.remove_file(Path::new("/etc/devbox/new/file.toml")).unwrap();
    assert!(!system.exists(Path::new("/etc/devbox/new/file.toml")));
    // commands: arguments, environment, input — and a working directory as `env -C`
    let command = dsys::Command::new("docker", ["version"])
        .in_dir(Path::new("/opt/services"))
        .env("X", "1")
        .stdin("in")
        .timeout(Duration::from_secs(5));
    let out = system.run(&command).unwrap();
    assert_eq!((out.code, out.text().as_str()), (Some(0), "29.1.0"));
    assert_eq!(s.fixture.commands_run().last().unwrap(), "env -C /opt/services docker version");
    assert_eq!(s.fixture.environment_of("env -C /opt/services docker"), [("X".to_string(), "1".to_string())]);
    let ran = s.machine.ran();
    assert_eq!(ran.last().unwrap().cwd.as_deref(), Some(Path::new("/opt/services")));
    // a unix connection: the fake Incus answers through it
    let mut conn = system.connect_unix(Path::new(INCUS)).unwrap();
    conn.write_all(b"GET /1.0 HTTP/1.1\r\nHost: incus\r\n\r\n").unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200") && answer.contains("\"server_version\":\"6.0.5\""), "{answer}");
    // the clock, the environment, randomness
    assert_eq!(system.now(), UNIX_EPOCH + Duration::from_secs(1_790_000_000));
    assert_eq!(system.env("SUDO_USER").as_deref(), Some("dev"));
    assert_eq!(system.random_bytes(16).unwrap().len(), 16);
    let before = system.monotonic();
    system.sleep(Duration::from_secs(3));
    assert!(system.monotonic() - before >= Duration::from_secs(3));
}

#[test]
fn without_devbox_the_commands_say_how_to_set_it_up() {
    let s = server("devbox-absent");
    let (code, out) = s.moi(&["devbox", "status"]);
    assert_eq!(code, 2);
    assert!(
        out.contains("devbox is not set up on this server (no /etc/devbox/devbox.toml)")
            && out.contains("moi devbox up --domain"),
        "{out}"
    );
    // and nothing of it is anywhere in moi's records
    assert!(!devbox::enabled(
        s.host.as_ref(),
        &cestmoi::config::load(s.host.as_ref(), &cestmoi::config::Paths::new("/etc/moi/moi.toml".into(), None))
            .unwrap()
            .config
    ));
}

#[test]
fn up_status_doctor_and_connect_print_what_devbox_prints() {
    let s = server("devbox-up");
    // the plan: devbox's, with the firewall step in moi's words, and nothing changed
    let (code, plan) = s.moi(&["devbox", "up", "--domain", "example.com", "--dry-run"]);
    assert_eq!(code, 0, "{plan}");
    common::assert_golden(&mine("plan-cestmoi.txt"), &plan);
    assert!(plan.contains(" 8. Firewall\n    + moi: containers → 10.77.0.10 tcp 22,8080,8090 in /etc/moi/firewall/rules4.txt (moi's rules file, applied by moi with its rollback)\n"), "{plan}");
    assert!(!s.host.exists(Path::new("/etc/devbox/devbox.toml")));
    // everything but the firewall step and the notice is devbox's own golden plan
    let theirs = golden("plan-fresh.txt");
    let same = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| {
                !l.contains("moi")
                    && !l.contains("left to you")
                    && !l.contains("changes.")
                    && !l.contains("Preflight")
                    && !l.trim_start().starts_with("ok: root")
            })
            .map(str::to_string)
            .collect()
    };
    assert_eq!(same(&plan), same(&theirs));

    let (code, out) = s.up();
    assert_eq!(code, 0, "{out}");
    // devbox's own golden file, but for the one line that says whose firewall it is
    assert_eq!(
        s.read("/etc/devbox/devbox.toml"),
        golden("devbox.toml").replace("manager = \"none\"", "manager = \"moi\"")
    );
    let secrets = s.host.metadata(Path::new("/etc/devbox/secrets.toml")).unwrap();
    assert_eq!((secrets.mode, secrets.uid, secrets.gid), (0o600, 65532, 65532));
    assert!(s.machine.incus(|incus| incus.instances.contains_key("devbox")));
    assert!(!out.contains("FAKE-wake-password") && !out.contains("FAKE-app-password"));

    // byte for byte devbox's own output
    assert_eq!(s.moi(&["devbox", "status", "--json"]), (0, golden("status.json")));
    assert_eq!(s.moi(&["devbox", "status"]), (0, golden("status.txt")));
    // doctor: devbox's golden was made on a host without a firewall manager, where that one check is
    // skipped; here it is moi's and it passes. Every other check is devbox's golden, byte for byte.
    let (code, doctor) = s.moi(&["devbox", "doctor", "--json"]);
    assert_eq!(code, 0, "{doctor}");
    common::assert_golden(&mine("doctor.json"), &doctor);
    let checks = |text: &str| -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        v["checks"].as_array().unwrap().iter().filter(|c| c["id"] != "firewall").cloned().collect()
    };
    assert_eq!(checks(&doctor), checks(&golden("doctor-healthy.json")));
    let ours: serde_json::Value = serde_json::from_str(&doctor).unwrap();
    let firewall = ours["checks"].as_array().unwrap().iter().find(|c| c["id"] == "firewall").unwrap();
    assert_eq!(firewall["result"], "ok", "{firewall}");
    let (code, text) = s.moi(&["devbox", "doctor"]);
    assert_eq!(code, 0, "{text}");
    common::assert_golden(&mine("doctor.txt"), &text);
    assert_eq!(s.moi(&["devbox", "connect"]), (0, golden("connect.txt")));

    // again: nothing to do, nothing asked
    let asked = s.fixture.asked().len();
    let (code, again) = s.moi(&["devbox", "up"]);
    assert_eq!(code, 0, "{again}");
    assert!(again.contains("\nEverything is already up to date.\n"), "{again}");
    assert!(again.contains(" 8. Firewall\n    up to date: moi: containers → 10.77.0.10"), "{again}");
    assert_eq!(s.fixture.asked().len(), asked);
}

const RULE: &str = "-A DOCKER-USER -s 172.16.0.0/12 -d 10.77.0.10/32 -p tcp -m multiport --dports 22,8080,8090 -m comment --comment \"devbox: proxies to VM\" -j ACCEPT";

fn firewall_commands(s: &Server) -> Vec<String> {
    s.fixture
        .commands_run()
        .into_iter()
        .filter(|c| c.contains("tables-restore") || c.starts_with("systemd-run"))
        .collect()
}

#[test]
fn the_rule_is_a_marked_block_in_mois_rules_file_applied_by_moi() {
    let s = server("devbox-firewall");
    let before = s.read("/etc/moi/firewall/rules4.txt");
    assert_eq!(s.up().0, 0);
    // in front of the first DOCKER-USER rule, so that it is reached before the chain's DROP
    let after = s.read("/etc/moi/firewall/rules4.txt");
    assert_eq!(
        after,
        before.replace(
            "-A DOCKER-USER -m comment",
            &format!("# devbox: begin\n{RULE}\n# devbox: end\n-A DOCKER-USER -m comment")
        )
    );
    // applied the way `moi firewall apply` applies: one transaction per family, the old rules saved
    let applied = firewall_commands(&s);
    assert_eq!(applied.iter().filter(|c| c.starts_with("iptables-restore --noflush")).count(), 1, "{applied:?}");
    assert!(
        s.host.read_dir(Path::new("/var/lib/moi/archive/firewall")).unwrap().iter().any(|f| f.ends_with(".v4.rules"))
    );
    assert_eq!(s.moi(&["firewall", "status"]).0, 0, "moi's own view: in sync");
    // the only firewall commands are moi's own apply: devbox ran none (no ufw, no nft, no `moi firewall apply`)
    let own: Vec<String> = s
        .fixture
        .commands_run()
        .into_iter()
        .filter(|c| c.starts_with("moi firewall apply") || c.starts_with("ufw allow") || c.starts_with("ufw route"))
        .collect();
    assert!(own.is_empty(), "{own:?}");

    // again: a no-op
    let commands = s.fixture.commands_run().len();
    assert_eq!(s.moi(&["devbox", "up", "--yes"]).0, 0);
    assert!(firewall_commands(&s).len() == applied.len() && s.read("/etc/moi/firewall/rules4.txt") == after);
    assert!(s.fixture.commands_run()[commands..].iter().all(|c| !c.contains("restore")));

    // a VM that moved: the block is replaced in place, not added twice
    let rule = devbox_core::firewall::Rule::new("10.77.0.99", &[22, 8080, 8090]);
    let moved = devbox::rules_with(&after, Some(&rule));
    assert_eq!(moved.matches("# devbox: begin").count(), 1);
    assert!(moved.contains("-d 10.77.0.99/32") && !moved.contains("10.77.0.10"));
    // a rule a standalone `devbox` left without the markers is taken into the block
    let unmarked = before.replace("-A DOCKER-USER -m comment", &format!("{RULE}\n-A DOCKER-USER -m comment"));
    assert_eq!(
        devbox::rules_with(&unmarked, Some(&devbox_core::firewall::Rule::new("10.77.0.10", &[22, 8080, 8090]))),
        after
    );
    assert_eq!(devbox::rules_with(&after, None), before);
}

#[test]
fn without_a_managed_firewall_moi_refuses_and_says_what_to_do() {
    let s = server("devbox-firewall-off");
    let config = s.read("/etc/moi/moi.toml").replace("[firewall]\nmanage = true", "[firewall]\nmanage = false");
    s.write("/etc/moi/moi.toml", &config, 0o644);
    let (code, out) = s.up();
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("moi does not manage this server's firewall (firewall.manage is off)")
            && out.contains("devbox up --firewall <host-firewall|ufw|nftables|none>"),
        "{out}"
    );
    assert!(!s.host.exists(Path::new("/etc/devbox/devbox.toml")), "nothing was set up");
    assert!(!s.machine.incus(|incus| incus.instances.contains_key("devbox")));
}

#[test]
fn over_ssh_the_rollback_is_armed_before_the_rule_is_applied() {
    let s = server("devbox-firewall-ssh");
    s.fixture.set_env("SSH_CONNECTION", "203.0.113.5 50000 192.0.2.10 22");
    s.fixture.add_command_then("systemd-run ...", 0, "");
    s.fixture.add_command("systemctl stop moi-firewall-rollback.timer", 0, "");
    s.fixture.answer("yes");
    let (code, out) = s.up();
    assert_eq!(code, 0, "{out}");
    let commands = firewall_commands(&s);
    let at = |needle: &str| {
        commands.iter().position(|c| c.starts_with(needle)).unwrap_or_else(|| panic!("{needle}: {commands:?}"))
    };
    assert!(
        at("systemd-run --quiet --collect --unit=moi-firewall-rollback") < at("iptables-restore --noflush"),
        "{commands:?}"
    );
    assert!(
        s.fixture
            .asked()
            .iter()
            .any(|q| q.starts_with("Rules applied. If you can still read this, type yes within 60 seconds"))
    );

    // not confirmed: the rules go back, and so does the file — devbox's step fails and says so
    let s = server("devbox-firewall-ssh-unconfirmed");
    s.fixture.set_env("SSH_CONNECTION", "203.0.113.5 50000 192.0.2.10 22");
    s.fixture.add_command_then("systemd-run ...", 0, "");
    s.fixture.add_command_then("iptables-restore ...", 0, "");
    s.fixture.add_command_then("ip6tables-restore ...", 0, "");
    s.fixture.add_command("systemctl stop moi-firewall-rollback.timer", 0, "");
    let before = s.read("/etc/moi/firewall/rules4.txt");
    let (code, out) = s.up();
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("not confirmed: the previous rules are back in place"), "{out}");
    assert_eq!(s.read("/etc/moi/firewall/rules4.txt"), before);
}

#[test]
fn down_takes_the_block_out_and_restores_what_up_wrote_and_both_are_recorded() {
    let s = server("devbox-down");
    let rules_before = s.read("/etc/moi/firewall/rules4.txt");
    let (code, out) = s.up();
    assert_eq!(code, 0, "{out}");
    // moi's records after `up`: devbox's identity among the backup paths, a snapshot, the context file
    assert!(out.contains("moi: /etc/devbox added to backup.paths in /etc/moi/moi.toml\nmoi: /var/lib/devbox/gateway added to backup.paths in /etc/moi/moi.toml\nmoi: snapshot and CONTEXT.md updated\n"), "{out}");
    let config = s.read("/etc/moi/moi.toml");
    assert!(config.contains("\"/etc/moi\", \"/etc/devbox\", \"/var/lib/devbox/gateway\"]   # devbox's identity, added by `moi devbox up`"), "{config}");
    assert_eq!(s.moi(&["config", "check", "--quiet"]).0, 0);
    assert!(s.host.exists(Path::new("/var/lib/moi/state/snapshot/meta/hostname")));
    // a second `up` adds nothing twice
    assert_eq!(s.moi(&["devbox", "up", "--yes"]).0, 0);
    assert_eq!(s.read("/etc/moi/moi.toml"), config);

    // what `down` would do, with nothing done
    let (code, would) = s.moi(&["devbox", "down", "--dry-run"]);
    assert_eq!(code, 0, "{would}");
    assert!(
        would.contains("/etc/devbox/devbox.toml") && s.host.exists(Path::new("/etc/devbox/devbox.toml")),
        "{would}"
    );
    assert!(s.read("/etc/moi/firewall/rules4.txt").contains("# devbox: begin"));

    let (code, out) = s.moi(&["devbox", "down", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(s.read("/etc/moi/firewall/rules4.txt"), rules_before, "the block is gone, the rest untouched");
    assert!(
        !s.host.exists(Path::new("/etc/devbox/devbox.toml"))
            && !s.host.exists(Path::new("/etc/dokploy/traefik/dynamic/devbox.yml"))
    );
    assert!(s.machine.incus(|incus| incus.instances.contains_key("devbox")), "the VM is kept unless --destroy");
    assert!(out.contains("moi: snapshot and CONTEXT.md updated\n"), "{out}");
    assert_eq!(s.moi(&["firewall", "status"]).0, 0);
}

#[test]
fn backup_paths_are_extended_the_way_people_write_arrays() {
    use cestmoi::devbox::records::with_paths;
    let extra = ["/etc/devbox", "/var/lib/devbox/gateway"];
    let one_line =
        "[backup]\nlocal_dir = \"/srv/b\"\npaths = [\"/opt\", \"/etc/moi\"]\n\n[backup.offsite]\nrepository = \"x\"\n";
    assert_eq!(
        with_paths(one_line, &extra).unwrap(),
        "[backup]\nlocal_dir = \"/srv/b\"\npaths = [\"/opt\", \"/etc/moi\", \"/etc/devbox\", \"/var/lib/devbox/gateway\"]   # devbox's identity, added by `moi devbox up`\n\n[backup.offsite]\nrepository = \"x\"\n"
    );
    let many_lines =
        "[backup]\npaths = [\n  \"/opt\",   # stacks ] and more\n  \"/etc/moi\",\n]\nexclude = [\"*/cache\"]\n";
    let extended = with_paths(many_lines, &extra).unwrap();
    assert!(
        extended.contains("  \"/etc/moi\",\n\"/etc/devbox\", \"/var/lib/devbox/gateway\"]   # devbox's identity")
            && extended.ends_with("exclude = [\"*/cache\"]\n"),
        "{extended}"
    );
    assert!(extended.parse::<toml::Table>().is_ok());
    assert!(with_paths(one_line, &extra).unwrap().parse::<toml::Table>().is_ok());
    assert_eq!(
        with_paths("[backup]\npaths = []\n", &["/etc/devbox"]).unwrap(),
        "[backup]\npaths = [\"/etc/devbox\"]   # devbox's identity, added by `moi devbox up`\n"
    );
    // no [backup], or no paths key: nothing is invented
    assert_eq!(with_paths("[server]\nname = \"x\"\n", &extra), None);
    assert_eq!(with_paths("[backup]\nlocal_dir = \"/srv/b\"\n", &extra), None);
}

// ------------------------------------------------------------------------------------------------ what moi observes

const KUMA_DB: &str = "/opt/services/monitoring/uptime-kuma/data/kuma.db";

fn config_of(s: &Server) -> cestmoi::config::Config {
    cestmoi::config::load(s.host.as_ref(), &cestmoi::config::Paths::new("/etc/moi/moi.toml".into(), None))
        .unwrap()
        .config
}

#[test]
fn the_health_monitor_is_adopted_by_name_drifts_on_its_url_and_is_created_when_missing() {
    use cestmoi::docker::Docker;
    use cestmoi::monitors::inventory::{Inventory, KumaMonitor};
    use cestmoi::monitors::{self, Action, Kind, Options};
    let s = server("devbox-monitor");
    assert_eq!(s.up().0, 0);
    let config = config_of(&s);
    let wanted = monitors::desired(&config, &Docker::new(s.host.clone()));
    let row = wanted.iter().find(|d| d.owner == "devbox").expect("a row for devbox");
    assert_eq!(
        (row.name.as_str(), row.kind, row.interval, row.retries, row.url.as_deref()),
        ("Devbox", Kind::Http, 60, 2, Some("https://code.example.com/__devbox/health"))
    );
    let plan = |monitors: Vec<KumaMonitor>, update: bool| {
        let inventory = Inventory { monitors, ..Inventory::default() };
        monitors::plan(&config, std::slice::from_ref(row), &inventory, &[], &[], Options { update, prune: false })
    };
    // missing: created, with what Kuma will ask
    let created = plan(Vec::new(), false);
    assert_eq!(
        (created.steps[0].action, created.steps[0].detail.as_str()),
        (Action::Create, "https://code.example.com/__devbox/health every 60s")
    );
    // there under that name: adopted; pointing at the old address, checking less often and paused is drift
    let old = KumaMonitor {
        id: 7,
        name: "Devbox".into(),
        kind: "http".into(),
        active: false,
        interval: Some(120),
        retries: Some(0),
        url: Some("http://10.77.0.10:8080/healthz".into()),
        ..KumaMonitor::default()
    };
    let adopted = plan(vec![old.clone()], false);
    assert_eq!((adopted.steps[0].action, adopted.steps[0].kuma_id), (Action::Adopt, Some(7)));
    assert_eq!(
        (adopted.steps[1].action, adopted.steps[1].detail.as_str()),
        (
            Action::Drift,
            "interval 120 (moi default 60), url http://10.77.0.10:8080/healthz (moi default https://code.example.com/__devbox/health), retries 0 (moi default 2), paused"
        )
    );
    assert_eq!(plan(vec![old.clone()], true).steps[1].action, Action::Update);
    // as it should be: adopted, and nothing else to say
    let right = KumaMonitor { active: true, interval: Some(60), retries: Some(2), url: row.url.clone(), ..old.clone() };
    assert_eq!(plan(vec![right], false).steps.len(), 1);
    // a monitor of another kind under that name is not taken over
    let push = KumaMonitor { kind: "push".into(), ..old };
    assert_eq!(plan(vec![push], false).steps[0].action, Action::Conflict);
    // no monitor wanted: no row
    s.write("/etc/moi/moi.toml", &format!("{}\n[devbox]\nmonitor = \"\"\n", s.read("/etc/moi/moi.toml")), 0o644);
    assert!(!monitors::desired(&config_of(&s), &Docker::new(s.host.clone())).iter().any(|d| d.owner == "devbox"));
}

#[test]
fn up_creates_or_repoints_the_monitor_in_kuma() {
    use cestmoi::monitors::inventory;
    let s = server("devbox-monitor-kuma");
    let kuma = common::fake_kuma::FakeKuma::new("1.23.17", "FAKE-kuma_username-value", "FAKE-kuma_password-value");
    kuma.seed(&inventory::read(s.host.as_ref(), KUMA_DB).unwrap());
    kuma.serve(&s.fixture);
    let (code, out) = s.up();
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(
            "moi: monitor \"Devbox\": create (https://code.example.com/__devbox/health every 60s)\nmoi: monitor done ("
        ),
        "{out}"
    );
    let monitor = kuma.monitor_named("Devbox").expect("the monitor exists in Kuma");
    assert_eq!(
        (
            monitor["type"].as_str(),
            monitor["url"].as_str(),
            monitor["interval"].as_u64(),
            monitor["maxretries"].as_u64()
        ),
        (Some("http"), Some("https://code.example.com/__devbox/health"), Some(60), Some(2))
    );
    assert_eq!(monitor["accepted_statuscodes"], serde_json::json!(["200-299"]));
    assert!(kuma.tags_of(monitor["id"].as_i64().unwrap()).contains(&"moi".to_string()), "tagged as moi's");
}

#[test]
fn snapshot_context_and_audit_describe_devbox_and_never_its_secrets() {
    let s = server("devbox-records");
    assert_eq!(s.up().0, 0);
    let secrets = s.read("/etc/devbox/secrets.toml");
    let snapshot = Path::new("/var/lib/moi/state/snapshot/devbox");
    assert_eq!(s.host.read_dir(snapshot).unwrap(), ["devbox.toml", "routing.yml", "status.json"]);
    assert_eq!(s.read("/var/lib/moi/state/snapshot/devbox/devbox.toml"), s.read("/etc/devbox/devbox.toml"));
    assert_eq!(
        s.read("/var/lib/moi/state/snapshot/devbox/routing.yml"),
        s.read("/etc/dokploy/traefik/dynamic/devbox.yml")
    );
    let status = s.read("/var/lib/moi/state/snapshot/devbox/status.json");
    common::assert_golden(&mine("snapshot-status.json"), &status);
    common::assert_golden(&mine("snapshot-state.json"), &s.read("/var/lib/moi/state/volatile/devbox/state.json"));
    // the secrets: that they exist and who may read them — not one value, anywhere in the state
    assert!(status.contains("\"mode\": \"0600\""), "{status}");
    for value in
        secrets.lines().filter_map(|l| l.split_once(" = ").map(|(_, v)| v.trim_matches('"'))).filter(|v| v.len() > 12)
    {
        for file in
            ["snapshot/devbox/status.json", "snapshot/devbox/devbox.toml", "volatile/devbox/state.json", "CONTEXT.md"]
        {
            assert!(!s.read(&format!("/var/lib/moi/state/{file}")).contains(value), "a secret of devbox is in {file}");
        }
    }
    assert!(
        !s.host.exists(Path::new("/var/lib/moi/state/snapshot/files/etc/devbox/secrets.toml")),
        "the secrets file is not copied into the state"
    );

    let context = s.read("/var/lib/moi/state/CONTEXT.md");
    let block: String =
        context.split("### devbox\n").nth(1).expect("a devbox block").split("\n## ").next().unwrap().to_string();
    common::assert_golden(&mine("context-devbox.md"), &block);
    assert!(
        block.contains("editor `https://code.example.com`")
            && block.contains("VM running")
            && block.contains("**Health:** ok"),
        "{block}"
    );

    // the audit: the configuration is covered (up added it to backup.paths), the disk is fungible
    s.moi(&["audit"]);
    let audit = s.read("/var/lib/moi/state/audit.md");
    let section: String =
        audit.split("\n## devbox\n").nth(1).expect("a devbox section").split("\n## ").next().unwrap().to_string();
    common::assert_golden(&mine("audit-devbox.md"), &section);
    assert!(
        section.contains("`/etc/devbox` is in the backup") && section.contains("fungible `/var/lib/incus`"),
        "{section}"
    );

    // problems are on the attention list
    s.machine.program("curl", |command| {
        let url = command.args.last().map(String::as_str).unwrap_or_default();
        url.ends_with("/__devbox/health").then(|| dsys::Output { code: Some(0), stdout: br#"{"v":1,"status":"degraded","problems":["the hub does not answer"],"vm":"running","phase":"idle","checkedAt":1790000000000}"#.to_vec(), ..dsys::Output::default() })
    });
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    let context = s.read("/var/lib/moi/state/CONTEXT.md");
    let attention = context.split("## ⚠ Attention").nth(1).unwrap().split("\n## ").next().unwrap().to_string();
    assert!(attention.contains("devbox: the hub does not answer"), "{attention}");
}

#[test]
fn an_uncovered_devbox_is_a_gap_and_an_undeclared_disk_a_question() {
    let s = server("devbox-audit-gaps");
    // devbox set up by its own CLI, moi told nothing: not in backup.paths, the disk not declared
    assert_eq!(s.up().0, 0);
    let config = s
        .read("/etc/moi/moi.toml")
        .replace(", \"/etc/devbox\", \"/var/lib/devbox/gateway\"]", "]")
        .replace("fungible = [\"/mnt/storage/media\", \"/var/lib/incus\"]", "fungible = [\"/mnt/storage/media\"]")
        .replace(", \"/var/lib/incus\" = \"devbox VM — rebuilt from github.com/prabhask5/dotfiles\"", "");
    s.write("/etc/moi/moi.toml", &config, 0o644);
    assert_eq!(s.moi(&["config", "check", "--quiet"]).0, 0);
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    assert_eq!(s.moi(&["audit"]).0, 1);
    let audit = s.read("/var/lib/moi/state/audit.md");
    let section: String = audit.split("\n## devbox\n").nth(1).unwrap().split("\n## ").next().unwrap().to_string();
    assert!(
        section.contains("✗ `/etc/devbox` is not in the backup")
            && section.contains("✗ `/var/lib/devbox/gateway` is not in the backup"),
        "{section}"
    );
    assert!(
        section.contains("review `/var/lib/incus` (the VM's disk) is neither backed up nor declared replaceable"),
        "{section}"
    );
}

#[test]
fn the_integration_is_on_by_itself_where_devbox_is_and_can_be_switched_off() {
    let s = server("devbox-config");
    assert!(!devbox::enabled(s.host.as_ref(), &config_of(&s)), "auto: off without /etc/devbox/devbox.toml");
    assert_eq!(s.up().0, 0);
    assert!(devbox::enabled(s.host.as_ref(), &config_of(&s)), "auto: on once devbox is set up");
    let base = s.read("/etc/moi/moi.toml");
    s.write("/etc/moi/moi.toml", &format!("{base}\n[devbox]\nenabled = false\n"), 0o644);
    assert!(!devbox::enabled(s.host.as_ref(), &config_of(&s)));
    let (code, out) = s.moi(&["devbox", "status"]);
    assert_eq!(code, 2);
    assert!(out.contains("switched off ([devbox] enabled = false"), "{out}");
    assert_eq!(s.moi(&["snapshot", "--quiet"]).0, 0);
    assert!(!s.host.exists(Path::new("/var/lib/moi/state/snapshot/devbox")), "switched off: nothing recorded");
    // what is not a setting is refused
    s.write("/etc/moi/moi.toml", &format!("{base}\n[devbox]\nenabled = \"sometimes\"\nmonitr = \"x\"\n"), 0o644);
    let (code, out) = s.moi(&["config", "check"]);
    assert_eq!(code, 2);
    assert!(out.contains("devbox.enabled: expected \"auto\", true or false") && out.contains("devbox.monitr"), "{out}");
}

#[test]
fn init_writes_the_section_and_declares_the_disk_only_where_devbox_is() {
    let s = server("devbox-init");
    let text = cestmoi::init::config_text(&cestmoi::init::discover(&s.host), false);
    assert!(!text.contains("[devbox]"), "{text}");
    assert_eq!(s.up().0, 0);
    let found = cestmoi::init::discover(&s.host);
    assert!(found.devbox && found.fungible.contains(&"/var/lib/incus".to_string()));
    let text = cestmoi::init::config_text(&found, false);
    assert!(text.contains("\n[devbox]\nenabled = \"auto\"\nmonitor = \"Devbox\"\n"), "{text}");
    assert!(
        text.contains("fungible_reason = { \"/var/lib/incus\" = \"devbox VM, rebuilt by `moi devbox up`\" }"),
        "{text}"
    );
    assert!(text.parse::<toml::Table>().is_ok());
}

#[test]
fn a_rebuild_recreates_the_vm_from_the_restored_configuration_without_a_question() {
    let s = server("devbox-restore");
    assert_eq!(s.up().0, 0);
    // the new machine: /etc/devbox and moi's rules file came back with the data, the VM's disk did not
    s.machine.incus(|incus| incus.instances.clear());
    s.fixture.add_command("incus list -c n -f csv", 0, "");
    s.host.create_dir_all(Path::new("/root/bundle/var/lib/moi/state/snapshot"), 0o700).unwrap();
    s.write("/root/bundle/etc/moi/moi.toml", &s.read("/etc/moi/moi.toml"), 0o644);
    s.write("/root/bundle/etc/moi/secrets.toml", &s.read("/etc/moi/secrets.toml"), 0o600);
    let asked = s.fixture.asked().len();
    let restores = firewall_commands(&s).len();

    let (code, plan) = s.moi(&["restore", "--bundle", "/root/bundle", "--only", "devbox", "--dry-run"]);
    assert_eq!(code, 0, "{plan}");
    assert!(
        plan.contains("the instance devbox does not exist: rebuilt from /etc/devbox")
            && plan.contains("     3. Incus instance\n        + create devbox:"),
        "{plan}"
    );
    assert!(!s.machine.incus(|incus| incus.instances.contains_key("devbox")), "a dry run creates nothing");

    let (code, out) = s.moi(&["restore", "--bundle", "/root/bundle", "--only", "devbox", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(s.machine.incus(|incus| incus.instances.contains_key("devbox")), "{out}");
    assert!(out.contains("  ✓ devbox answers: health ok."), "{out}");
    assert_eq!(s.fixture.asked().len(), asked, "no password and no confirmation was asked for");
    // the rule is in moi's file already; applying it is the firewall phase's job
    assert!(s.read("/etc/moi/firewall/rules4.txt").contains("# devbox: begin"));
    assert_eq!(firewall_commands(&s).len(), restores, "{:?}", firewall_commands(&s));

    // a machine whose disk came along: adopted, nothing recreated
    s.fixture.add_command("incus list -c n -f csv", 0, "devbox\n");
    let (code, out) = s.moi(&["restore", "--bundle", "/root/bundle", "--only", "devbox", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("the instance devbox exists: adopted, its disk and data kept"), "{out}");
    // the phase comes after the VM host and the stacks, and before the firewall
    let phases: Vec<&str> = cestmoi::restore::PHASES.iter().map(|(name, _)| *name).collect();
    let at = |name: &str| phases.iter().position(|p| *p == name).unwrap();
    assert!(at("incus") < at("devbox") && at("docker") < at("devbox") && at("devbox") < at("firewall"));
}

#[test]
fn a_vm_that_exists_is_adopted_not_recreated() {
    use devbox_core::fake::FakeInstance;
    let s = server("devbox-adopt");
    // cestmoi's own situation: the instance `devbox` is in Incus already
    s.machine.incus(|incus| {
        incus.instances.insert("devbox".to_owned(), FakeInstance::stopped());
    });
    // without being told, devbox does not take over an instance it did not create
    let (code, refused) = s.moi(&["devbox", "up", "--domain", "example.com", "--dry-run"]);
    assert_eq!(code, 2, "{refused}");
    assert!(refused.contains("--adopt"), "{refused}");
    let (code, plan) = s.moi(&["devbox", "up", "--domain", "example.com", "--adopt", "--dry-run"]);
    assert_eq!(code, 0, "{plan}");
    common::assert_golden(&mine("plan-adopt.txt"), &plan);
    assert!(plan.contains(" 3. Incus instance\n") && !plan.contains("+ create devbox"), "{plan}");
    assert!(
        plan.contains("/etc/moi/firewall/rules4.txt (moi's rules file, applied by moi with its rollback)"),
        "{plan}"
    );
}
