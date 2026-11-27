//! The host firewall and the boot tasks (docs/spec/10-daemon.md § Firewall, boot).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cestmoi::cli;
use cestmoi::config::{self, Config, Paths};
use cestmoi::daemon::Daemon;
use cestmoi::firewall::{self, Family};
use cestmoi::host::{FixtureHost, Host};

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
}

fn server(test: &str) -> Server {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    Server { host: fixture.clone(), fixture }
}

const SAVED_V4: &str = "/var/lib/moi/archive/firewall/2026-11-26T170000Z.v4.rules";
const SAVED_V6: &str = "/var/lib/moi/archive/firewall/2026-11-26T170000Z.v6.rules";

impl Server {
    fn config(&self) -> Config {
        config::load(self.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap().config
    }

    fn moi(&self, args: &[&str]) -> (u8, String) {
        let mut out = Vec::new();
        match cli::execute(&cli::parse(args).unwrap(), self.host.clone(), &mut out) {
            Ok(()) => (0, String::from_utf8(out).unwrap()),
            Err(e) if e.exit_code() == 1 => (1, String::from_utf8(out).unwrap()),
            Err(e) => (e.exit_code(), e.to_string()),
        }
    }

    fn edit(&self, path: &str, edit: impl FnOnce(String) -> String) {
        let text = edit(self.host.read_to_string(Path::new(path)).unwrap());
        self.host.write(Path::new(path), text.as_bytes(), 0o644).unwrap();
    }

    /// Commands run that touch the firewall, in order.
    fn firewall_commands(&self) -> Vec<String> {
        self.fixture
            .commands_run()
            .into_iter()
            .filter(|c| c.contains("tables") || c.starts_with("systemd-run") || c.starts_with("systemctl"))
            .collect()
    }

    /// What `iptables-restore --noflush` was given, per run.
    fn restored(&self, tool: &str) -> Vec<String> {
        self.fixture
            .inputs()
            .into_iter()
            .filter(|(line, _)| line == &format!("{tool}-restore --noflush"))
            .map(|(_, input)| input)
            .collect()
    }

    fn daemon(&self) -> Daemon {
        let loaded = config::load(self.host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
        Daemon::new(self.host.clone(), loaded).unwrap()
    }
}

const V4_INPUT: &str = "*filter\n:DOCKER-USER - [0:0]\n-F INPUT\n-A INPUT -i lo -j ACCEPT\n-A INPUT -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT\n-A INPUT -p tcp --dport 22 -j ACCEPT\n-A INPUT -j DROP\n-A DOCKER-USER -m comment --comment \"Intra-network container traffic\" -j RETURN\n-A DOCKER-USER -j DROP\nCOMMIT\n";

#[test]
fn the_restore_input_replaces_exactly_the_owned_chains_in_one_transaction() {
    let s = server("firewall-input");
    let config = s.config();
    let rules = firewall::rules(s.host.as_ref(), &config, Family::V4).unwrap();
    assert_eq!(rules.len(), 6);
    // user chains are declared (created if missing, emptied), built-in ones flushed, then all rules
    assert_eq!(firewall::restore_input(&config.firewall.chains, &rules), V4_INPUT);
    // a subset (at boot, before Docker is up): nothing about the other chain
    assert_eq!(
        firewall::restore_input(&["INPUT".to_string()], &rules),
        "*filter\n-F INPUT\n-A INPUT -i lo -j ACCEPT\n-A INPUT -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT\n-A INPUT -p tcp --dport 22 -j ACCEPT\n-A INPUT -j DROP\nCOMMIT\n"
    );
    assert!(firewall::builtin("INPUT") && !firewall::builtin("DOCKER-USER"));
}

#[test]
fn a_rules_file_may_only_fill_the_chains_moi_owns() {
    let s = server("firewall-rules");
    let rules = |text: &str| {
        s.host.write(Path::new("/etc/moi/firewall/rules4.txt"), text.as_bytes(), 0o644).unwrap();
        firewall::rules(s.host.as_ref(), &s.config(), Family::V4).map_err(|e| e.to_string())
    };
    // comments, blank lines, inserts and a policy for a built-in chain are fine
    assert_eq!(
        rules("# ssh\n\n-A INPUT -p tcp --dport 22 -j ACCEPT\n-I DOCKER-USER -j RETURN\n-P INPUT DROP\n")
            .unwrap()
            .len(),
        3
    );
    let err = rules("-A INPUT -j ACCEPT\n-A FORWARD -j DROP\n").unwrap_err();
    assert_eq!(
        err,
        "/etc/moi/firewall/rules4.txt:2: chain FORWARD is not one moi manages (firewall.chains = [\"INPUT\", \"DOCKER-USER\"])"
    );
    assert!(rules("-A DOCKER -j ACCEPT\n").unwrap_err().contains("chain DOCKER is not one moi manages"));
    assert!(rules("*nat\n").unwrap_err().contains(":1: only -A, -I and -P lines belong in a rules file"));
    assert!(rules("-F INPUT\n").unwrap_err().contains("only -A, -I and -P lines"));
    assert!(rules("-P DOCKER-USER DROP\n").unwrap_err().contains("only built-in chains have a policy"));
    // a file with a mistake means nothing is applied, for either family
    let (code, err) = s.moi(&["firewall", "apply"]);
    assert_eq!(code, 2);
    assert!(err.contains("only built-in chains have a policy"), "{err}");
    assert_eq!(s.firewall_commands(), Vec::<String>::new());
}

#[test]
fn apply_saves_what_was_there_and_touches_nothing_but_the_owned_chains() {
    let s = server("firewall-apply");
    // before: moi has never applied anything
    let (code, out) = s.moi(&["firewall", "status"]);
    assert_eq!(
        (code, out.as_str()),
        (1, "managed chains: INPUT, DOCKER-USER\nv4: never applied by moi\nv6: never applied by moi\n")
    );

    let looked = s.firewall_commands().len();
    let (code, out) = s.moi(&["firewall", "apply"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out,
        "applied 6 rules (v4), 2 rules (v6) to INPUT, DOCKER-USER\nprevious rules saved in /var/lib/moi/archive/firewall\n"
    );
    // saved first, then one transaction per family; no flush of anything else, no rollback machinery at the console
    assert_eq!(
        s.firewall_commands()[looked..],
        [
            "iptables-save",
            "ip6tables-save",
            "iptables-restore --noflush",
            "iptables -S INPUT",
            "iptables -S DOCKER-USER",
            "ip6tables-restore --noflush",
            "ip6tables -S INPUT",
            "ip6tables -S DOCKER-USER"
        ]
    );
    assert_eq!(s.restored("iptables"), [V4_INPUT]);
    assert_eq!(
        s.restored("ip6tables"),
        ["*filter\n:DOCKER-USER - [0:0]\n-F INPUT\n-A INPUT -i lo -j ACCEPT\n-A INPUT -j DROP\nCOMMIT\n"]
    );
    for input in s.restored("iptables") {
        assert!(
            !input.contains("DOCKER-ISOLATION")
                && !input.contains("f2b")
                && !input.contains("*nat")
                && !input.contains("FORWARD"),
            "{input}"
        );
    }
    // what was there before is kept, private to root
    let saved = s.host.read_to_string(Path::new(SAVED_V4)).unwrap();
    assert!(
        saved.contains(":f2b-sshd - [0:0]") && saved.contains("*filter"),
        "the whole rule set, other chains included"
    );
    assert_eq!(s.host.metadata(Path::new(SAVED_V4)).unwrap().mode, 0o600);
    assert!(s.host.exists(Path::new(SAVED_V6)));

    let (code, out) = s.moi(&["firewall", "status"]);
    assert_eq!((code, out.as_str()), (0, "managed chains: INPUT, DOCKER-USER\nv4: in sync\nv6: in sync\n"));
    assert_eq!(
        s.moi(&["firewall", "diff"]),
        (
            0,
            "no difference: the managed chains are what moi applied, and the rules files have not changed\n"
                .to_string()
        )
    );
}

#[test]
fn status_and_diff_tell_a_changed_file_from_changed_live_rules() {
    let s = server("firewall-drift");
    // before the first apply: the live chains against the file
    let (code, out) = s.moi(&["firewall", "diff"]);
    assert_eq!(code, 1);
    assert!(out.starts_with("--- v4: live\n+++ v4: /etc/moi/firewall/rules4.txt\n@@"), "{out}");
    assert_eq!(s.moi(&["firewall", "apply"]).0, 0);

    // the owner edits the rules file
    s.edit("/etc/moi/firewall/rules4.txt", |t| t.replace("--dport 22", "--dport 2222"));
    let (code, out) = s.moi(&["firewall", "status"]);
    assert_eq!(
        (code, out.lines().nth(1).unwrap()),
        (1, "v4: rules file changed since it was applied (moi firewall apply)")
    );
    let (_, out) = s.moi(&["firewall", "diff"]);
    assert!(out.starts_with("v4: /etc/moi/firewall/rules4.txt changed since it was applied; `moi firewall apply` replaces the chains with:\n  -A INPUT -i lo -j ACCEPT\n"), "{out}");
    assert!(out.contains("  -A INPUT -p tcp --dport 2222 -j ACCEPT\n"));
    s.edit("/etc/moi/firewall/rules4.txt", |t| t.replace("--dport 2222", "--dport 22"));
    assert_eq!(s.moi(&["firewall", "status"]).0, 0);

    // someone (or something) changes the live chain behind moi's back
    s.fixture.add_command(
        "iptables -S INPUT",
        0,
        "-P INPUT ACCEPT\n-A INPUT -i lo -j ACCEPT\n-A INPUT -p tcp -m tcp --dport 8080 -j ACCEPT\n-A INPUT -j DROP\n",
    );
    let (code, out) = s.moi(&["firewall", "status", "--json"]);
    assert_eq!(code, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["families"],
        serde_json::json!([{ "family": "v4", "state": "drifted" }, { "family": "v6", "state": "in_sync" }])
    );
    let (code, out) = s.moi(&["firewall", "diff"]);
    assert_eq!(code, 1);
    assert!(out.starts_with("--- v4: applied by moi\n+++ v4: live\n"), "{out}");
    assert!(
        out.contains("+-A INPUT -p tcp -m tcp --dport 8080 -j ACCEPT\n")
            && out.contains("--A INPUT -p tcp -m tcp --dport 22 -j ACCEPT\n"),
        "{out}"
    );
}

fn rollback_timer(s: &Server, status: i32) {
    s.fixture.add_command(
        &format!("systemd-run --quiet --collect --unit=moi-firewall-rollback --on-active=90s /bin/sh -c iptables-restore '{SAVED_V4}'; ip6tables-restore '{SAVED_V6}'"),
        status,
        "",
    );
    s.fixture.add_command("systemctl stop moi-firewall-rollback.timer", 0, "");
    s.fixture.add_command(&format!("iptables-restore {SAVED_V4}"), 0, "");
    s.fixture.add_command(&format!("ip6tables-restore {SAVED_V6}"), 0, "");
}

#[test]
fn over_ssh_the_way_back_is_arranged_before_anything_changes() {
    let s = server("firewall-ssh-yes");
    s.fixture.set_env("SSH_CONNECTION", "203.0.113.5 50000 192.0.2.10 22");
    rollback_timer(&s, 0);
    s.fixture.answer("yes");
    let (code, out) = s.moi(&["firewall", "apply"]);
    assert_eq!(code, 0, "{out}");
    let commands = s.firewall_commands();
    let position = |needle: &str| {
        commands.iter().position(|c| c.starts_with(needle)).unwrap_or_else(|| panic!("{needle} not run: {commands:?}"))
    };
    assert!(position("iptables-save") < position("systemd-run"), "saved before the timer is armed");
    assert!(
        position("systemd-run") < position("iptables-restore --noflush"),
        "the rollback is scheduled before any rule changes"
    );
    assert_eq!(
        commands.last().unwrap(),
        "systemctl stop moi-firewall-rollback.timer",
        "confirmed: the timer is disarmed"
    );
    assert!(!commands.iter().any(|c| c == &format!("iptables-restore {SAVED_V4}")), "nothing was rolled back");
    assert_eq!(
        s.fixture.asked(),
        [
            "Rules applied. If you can still read this, type yes within 60 seconds to keep them; otherwise they are rolled back: "
        ]
    );
    assert_eq!(s.moi(&["firewall", "status"]).0, 0);
}

#[test]
fn unconfirmed_rules_are_rolled_back() {
    for (test, answer) in [("firewall-ssh-silence", None), ("firewall-ssh-no", Some("no"))] {
        let s = server(test);
        s.fixture.set_env("SSH_TTY", "/dev/pts/0");
        rollback_timer(&s, 0);
        if let Some(answer) = answer {
            s.fixture.answer(answer);
        }
        let before = s.host.now();
        let (code, err) = s.moi(&["firewall", "apply"]);
        assert_eq!((code, err.as_str()), (2, "not confirmed: the previous rules are back in place"));
        let commands = s.firewall_commands();
        let tail: Vec<&str> = commands.iter().rev().take(3).rev().map(String::as_str).collect();
        assert_eq!(
            tail,
            [
                format!("iptables-restore {SAVED_V4}").as_str(),
                format!("ip6tables-restore {SAVED_V6}").as_str(),
                "systemctl stop moi-firewall-rollback.timer"
            ]
        );
        if answer.is_none() {
            assert_eq!(
                s.host.now().duration_since(before).unwrap(),
                Duration::from_secs(60),
                "it waited the full minute for an answer"
            );
        }
    }
}

#[test]
fn without_a_way_back_nothing_is_applied_over_ssh() {
    let s = server("firewall-ssh-no-timer");
    s.fixture.set_env("SSH_CONNECTION", "203.0.113.5 50000 192.0.2.10 22");
    rollback_timer(&s, 1);
    let (code, err) = s.moi(&["firewall", "apply"]);
    assert_eq!(code, 2);
    assert!(
        err.starts_with("not applying over SSH without a way back: systemd-run could not schedule the rollback"),
        "{err}"
    );
    assert!(s.restored("iptables").is_empty() && s.restored("ip6tables").is_empty());
    assert!(s.fixture.asked().is_empty());
}

#[test]
fn a_family_that_fails_takes_the_other_back_with_it() {
    let s = server("firewall-apply-fails");
    s.fixture.add_command("ip6tables-restore --noflush", 2, "");
    s.fixture.add_command(&format!("iptables-restore {SAVED_V4}"), 0, "");
    s.fixture.add_command(&format!("ip6tables-restore {SAVED_V6}"), 0, "");
    let (code, err) = s.moi(&["firewall", "apply"]);
    assert_eq!(code, 2);
    assert!(
        err.starts_with("ip6tables-restore refused the rules in /etc/moi/firewall/rules6.txt (nothing was changed)"),
        "{err}"
    );
    let commands = s.firewall_commands();
    assert!(
        commands.contains(&format!("iptables-restore {SAVED_V4}")),
        "IPv4 was applied alone: put back. {commands:?}"
    );
}

#[test]
fn a_firewall_moi_does_not_manage_is_left_alone() {
    let s = server("firewall-unmanaged");
    s.edit("/etc/moi/moi.toml", |t| t.replace("manage = true", "manage = false"));
    assert_eq!(
        s.moi(&["firewall", "status"]),
        (0, "the firewall is not managed by moi (firewall.manage = false in moi.toml)\n".to_string())
    );
    let (code, err) = s.moi(&["firewall", "apply"]);
    assert_eq!((code, err.as_str()), (2, "set firewall.manage = true and the rules files in moi.toml first"));
    assert_eq!(s.firewall_commands(), Vec::<String>::new());
    assert!(!s.daemon().jobs().iter().any(|j| j.name == "firewall"));
}

// ------------------------------------------------------------------ boot

fn boot_outcome(daemon: &mut Daemon) -> Option<(String, String)> {
    daemon.tick().unwrap().into_iter().find(|(name, _)| name == "boot").map(|(_, o)| (o.status, o.message))
}

#[test]
fn the_first_start_after_a_boot_applies_the_firewall_and_starts_missing_stacks() {
    let s = server("boot-first");
    let mut daemon = s.daemon();
    let (status, message) = boot_outcome(&mut daemon).expect("this is the first start since the machine booted");
    assert_eq!(
        (status.as_str(), message.as_str()),
        ("ok", "firewall: 6 rules in INPUT; firewall: 8 rules in INPUT, DOCKER-USER; 1 stacks started")
    );
    // INPUT at once (before waiting for anything), everything once Docker is there
    let v4 = s.restored("iptables");
    assert_eq!(v4.len(), 2);
    assert!(v4[0].starts_with("*filter\n-F INPUT\n") && !v4[0].contains("DOCKER-USER"), "{}", v4[0]);
    assert_eq!(v4[1], V4_INPUT);
    // only the stack that is not up is started; running ones are left alone
    let composed: Vec<String> =
        s.fixture.commands_run().into_iter().filter(|c| c.starts_with("docker compose")).collect();
    assert_eq!(composed, ["docker compose --project-directory /opt/services/old-wiki up -d"]);

    // the service restarts later the same day: not a boot
    assert_eq!(boot_outcome(&mut s.daemon()), None);
    assert_eq!(s.restored("iptables").len(), 2);
    // the machine reboots
    s.host
        .write(Path::new("/proc/sys/kernel/random/boot_id"), b"11111111-2222-3333-4444-555555555555\n", 0o644)
        .unwrap();
    assert!(boot_outcome(&mut s.daemon()).is_some());
}

#[test]
fn stacks_are_not_started_while_their_data_is_missing() {
    // the USB drive did not come up
    let case = common::scratch("boot-no-mount-case");
    let mountinfo = std::fs::read_to_string(common::fixture_dir("cestmoi").join("root/proc/self/mountinfo")).unwrap();
    std::fs::create_dir_all(case.join("root/proc/self")).unwrap();
    std::fs::write(
        case.join("root/proc/self/mountinfo"),
        mountinfo.lines().filter(|l| !l.contains(" /mnt/storage ")).map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    let overlay = common::scratch("boot-no-mount");
    let fixture = Arc::new(
        FixtureHost::open(&common::fixture_dir("cestmoi"))
            .unwrap()
            .with_case(&case)
            .unwrap()
            .with_overlay(&overlay)
            .unwrap(),
    );
    let s = Server { host: fixture.clone(), fixture };
    s.fixture.set_env("JOURNAL_STREAM", "8:1");
    let mut daemon = s.daemon();
    let ran = daemon.tick().unwrap();
    let boot = &ran.iter().find(|(name, _)| name == "boot").unwrap().1;
    assert_eq!(boot.status, "failed");
    assert_eq!(
        boot.message,
        "firewall: 6 rules in INPUT; firewall: 8 rules in INPUT, DOCKER-USER; /mnt/storage is not mounted after 5 minutes; stacks not started: their data is not mounted"
    );
    assert!(
        !s.fixture.commands_run().iter().any(|c| c.starts_with("docker compose")),
        "a stack must not create its data on the wrong disk"
    );
    let warned: Vec<String> =
        s.fixture.journal_entries().iter().map(|e| e[0].1.clone()).filter(|m| m.starts_with("boot: ")).collect();
    assert_eq!(
        warned,
        ["boot: /mnt/storage is not mounted after 5 minutes", "boot: stacks not started: their data is not mounted"]
    );
    // this boot is dealt with: a restart of the service does not wait five minutes again
    assert_eq!(boot_outcome(&mut s.daemon()), None);
}

#[test]
fn the_managed_chains_are_put_back_when_docker_restarts() {
    let s = server("boot-docker-back");
    let mut daemon = s.daemon();
    daemon.tick().unwrap();
    let before = s.restored("iptables").len();
    assert!(!daemon.tick().unwrap().iter().any(|(name, _)| name == "firewall"), "not without a reason");
    daemon.docker_back();
    let ran = daemon.tick().unwrap();
    let firewall = &ran.iter().find(|(name, _)| name == "firewall").expect("Docker restarted: its chains may be new").1;
    assert_eq!(
        (firewall.status.as_str(), firewall.message.as_str()),
        ("ok", "firewall: 8 rules in INPUT, DOCKER-USER")
    );
    assert_eq!(s.restored("iptables").len(), before + 1);
    assert_eq!(s.restored("iptables").last().unwrap(), V4_INPUT);
}

#[test]
fn compose_directories_are_found_in_order() {
    let s = server("boot-compose-dirs");
    let dirs = |entry: &str| cestmoi::daemon::compose_dirs(s.host.as_ref(), entry);
    // `/**`: every directory below that has a compose file, sorted
    let found = dirs("/opt/services/**");
    assert!(
        found.contains(&"/opt/services/monitoring/uptime-kuma".to_string())
            && found.contains(&"/opt/services/old-wiki".to_string()),
        "{found:?}"
    );
    let mut sorted = found.clone();
    sorted.sort();
    assert_eq!(found, sorted);
    assert!(!found.contains(&"/opt/services/monitoring".to_string()), "a directory of stacks is not a stack");
    // a plain entry is that directory, if it is a stack
    assert_eq!(dirs("/opt/yams"), ["/opt/yams"]);
    assert_eq!(dirs("/opt/scripts"), Vec::<String>::new());
    assert_eq!(dirs("/nowhere/**"), Vec::<String>::new());
}

// ------------------------------------------------------------------ against real iptables (CI)

/// Run a shell script as root in a fresh network namespace: real netfilter, and nothing of the
/// machine's own firewall in reach.
fn in_new_netns(script: &str) -> (bool, String) {
    let out = std::process::Command::new("sudo")
        .args(["unshare", "--net", "sh", "-euc", script])
        .output()
        .expect("sudo unshare");
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// What moi generates is given to the real `iptables-restore --noflush`, next to chains and tables that
/// are not moi's — the claim "exactly the owned chains, in one transaction" checked where it counts.
#[test]
#[ignore = "needs MOI_IT services (root and iptables: runs in a throwaway network namespace)"]
fn real_iptables_replaces_only_the_owned_chains_and_all_or_nothing() {
    let s = server("firewall-real");
    let config = s.config();
    let rules = firewall::rules(s.host.as_ref(), &config, Family::V4).unwrap();
    let input = firewall::restore_input(&config.firewall.chains, &rules);
    let dir = common::scratch("firewall-real-files");
    std::fs::write(dir.join("moi.rules"), &input).unwrap();
    // a rule the kernel refuses, after good ones
    std::fs::write(
        dir.join("bad.rules"),
        input.replace("COMMIT\n", "-A INPUT -p tcp --dport notaport -j ACCEPT\nCOMMIT\n"),
    )
    .unwrap();
    // only INPUT, as at boot before Docker exists
    std::fs::write(dir.join("boot.rules"), firewall::restore_input(&["INPUT".to_string()], &rules)).unwrap();
    let d = dir.display();

    // a machine with Docker-like chains, a stale rule set in the owned chains, another table, a policy
    let setup = "iptables -N DOCKER; iptables -N DOCKER-USER; iptables -N f2b-sshd
        iptables -A FORWARD -j DOCKER-USER; iptables -A FORWARD -j DOCKER
        iptables -A DOCKER -p tcp --dport 8080 -j ACCEPT
        iptables -A DOCKER-USER -s 10.9.9.9 -j DROP
        iptables -A f2b-sshd -s 198.51.100.7 -j REJECT
        iptables -A INPUT -p tcp --dport 9999 -j ACCEPT; iptables -A INPUT -j f2b-sshd
        iptables -P FORWARD DROP
        iptables -t nat -A POSTROUTING -s 172.17.0.0/16 -j MASQUERADE";
    let (ok, out) = in_new_netns(&format!(
        "{setup}\niptables-restore --noflush < {d}/moi.rules\necho ---filter; iptables -S\necho ---nat; iptables -t nat -S"
    ));
    assert!(ok, "{out}");
    let (filter, nat) = out.split_once("---nat\n").unwrap();
    let lines: Vec<&str> = filter.lines().collect();
    // the owned chains hold exactly the rules from the file, in order
    let chain =
        |name: &str| -> Vec<&str> { lines.iter().copied().filter(|l| l.starts_with(&format!("-A {name} "))).collect() };
    assert_eq!(chain("INPUT").len(), 4, "{filter}");
    assert_eq!(chain("INPUT")[0], "-A INPUT -i lo -j ACCEPT");
    assert_eq!(*chain("INPUT").last().unwrap(), "-A INPUT -j DROP");
    assert!(
        !filter.contains("9999") && !filter.contains("-A INPUT -j f2b-sshd"),
        "stale INPUT rules are gone: {filter}"
    );
    assert_eq!(
        chain("DOCKER-USER"),
        ["-A DOCKER-USER -m comment --comment \"Intra-network container traffic\" -j RETURN", "-A DOCKER-USER -j DROP"]
    );
    // everything else is exactly as it was
    assert_eq!(chain("DOCKER"), ["-A DOCKER -p tcp -m tcp --dport 8080 -j ACCEPT"]);
    assert_eq!(chain("f2b-sshd"), ["-A f2b-sshd -s 198.51.100.7/32 -j REJECT --reject-with icmp-port-unreachable"]);
    assert_eq!(chain("FORWARD"), ["-A FORWARD -j DOCKER-USER", "-A FORWARD -j DOCKER"]);
    assert!(
        lines.contains(&"-P FORWARD DROP") && lines.contains(&"-P INPUT ACCEPT"),
        "policies nobody named are untouched: {filter}"
    );
    assert!(nat.contains("-A POSTROUTING -s 172.17.0.0/16 -j MASQUERADE"), "{nat}");

    // one bad rule: the transaction fails and not a single rule changed
    let (ok, out) = in_new_netns(&format!(
        "{setup}\niptables -S > /tmp/moi-before.$$\nif iptables-restore --noflush < {d}/bad.rules; then echo APPLIED; fi\niptables -S > /tmp/moi-after.$$\ncmp /tmp/moi-before.$$ /tmp/moi-after.$$ && echo UNCHANGED\nrm -f /tmp/moi-before.$$ /tmp/moi-after.$$"
    ));
    assert!(ok && out.contains("UNCHANGED") && !out.contains("APPLIED"), "{out}");

    // INPUT alone, on a machine where DOCKER-USER does not exist yet: it is neither created nor needed
    let (ok, out) = in_new_netns(&format!(
        "iptables -A INPUT -p tcp --dport 9999 -j ACCEPT\niptables-restore --noflush < {d}/boot.rules\niptables -S"
    ));
    assert!(ok, "{out}");
    assert!(out.contains("-A INPUT -j DROP") && !out.contains("9999") && !out.contains("DOCKER-USER"), "{out}");
    // and the full set on that machine creates the missing chain
    let (ok, out) = in_new_netns(&format!("iptables-restore --noflush < {d}/moi.rules\niptables -S DOCKER-USER"));
    assert!(ok && out.contains("-N DOCKER-USER") && out.contains("-A DOCKER-USER -j DROP"), "{out}");
}
