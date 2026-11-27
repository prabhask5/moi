//! `moi verify`: is everything that should run, running? What "should" means is the *expectations* —
//! what the last snapshot saw (and, after a rebuild, what the old server's last snapshot saw) — kept in
//! `expect.json` so that a reboot is judged against the machine as it was before it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::checks::{self, Mode, Status};
use crate::config::Config;
use crate::docker::Docker;
use crate::error::Error;
use crate::host::{Cmd, Host};
use crate::state::StateDir;
use crate::state::db::Db;

/// What should be there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expect {
    /// Containers that were running (swarm tasks are their service's business).
    pub containers: Vec<String>,
    /// Swarm services and how many replicas each wanted.
    pub services: Vec<(String, u64)>,
    /// `tcp 22`: listening ports below the ephemeral range.
    pub ports: Vec<String>,
    /// Mount points of real filesystems.
    pub mounts: Vec<String>,
    /// systemd services that were running.
    pub units: Vec<String>,
    /// Timers that were enabled.
    pub timers: Vec<String>,
    /// Users who had a crontab, and files in `/etc/cron.d`.
    pub cron: Vec<String>,
}

fn lines(host: &dyn Host, path: PathBuf) -> Vec<String> {
    host.read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// `tcp 0.0.0.0:22 …` → `tcp 22`, unless the port is in the range the kernel hands out by itself.
fn port(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    let (proto, address) = (words.next()?, words.next()?);
    let port: u32 = address.rsplit(':').next()?.parse().ok()?;
    (port < 32768).then(|| format!("{proto} {port}"))
}

impl Expect {
    pub fn path(state: &StateDir) -> PathBuf {
        state.root().join("expect.json")
    }

    /// What a state directory's snapshot saw.
    pub fn from_state(host: &dyn Host, state: &StateDir) -> Self {
        let (snapshot, volatile) = (state.snapshot(), state.volatile());
        let services: Vec<(String, u64)> = lines(host, volatile.join("service-ls.tsv"))
            .iter()
            .filter_map(|l| {
                let mut cells = l.split('\t');
                Some((cells.next()?.to_string(), cells.next()?.parse().ok()?))
            })
            .collect();
        // a task container is `<service>.<slot>.<task id>`: it comes back under another name
        let task = |name: &str| services.iter().any(|(service, _)| name.starts_with(&format!("{service}.")));
        let containers = lines(host, volatile.join("docker-ps.tsv"))
            .iter()
            .filter_map(|l| {
                let mut cells = l.split('\t');
                let (name, state) = (cells.next()?, cells.next()?);
                (state == "running" && !task(name)).then(|| name.to_string())
            })
            .collect();
        let ports: BTreeSet<String> =
            lines(host, volatile.join("listening.txt")).iter().filter_map(|l| port(l)).collect();
        let real = |l: &String| {
            let mut words = l.split_whitespace();
            let (target, fstype) = (words.next()?, words.nth(1)?);
            (!["tmpfs", "overlay", "squashfs", "proc", "sysfs", "devtmpfs"].contains(&fstype))
                .then(|| target.to_string())
        };
        let mut cron: Vec<String> = host
            .read_dir(&snapshot.join("cron"))
            .unwrap_or_default()
            .iter()
            .filter_map(|f| {
                f.strip_prefix("user-")
                    .and_then(|u| u.strip_suffix(".crontab"))
                    .map(|user| format!("crontab of {user}"))
            })
            .collect();
        cron.extend(
            host.read_dir(&snapshot.join("files/etc/cron.d"))
                .unwrap_or_default()
                .into_iter()
                .map(|f| format!("/etc/cron.d/{f}")),
        );
        // Only what boot starts is held to running: a service that the bus or a socket starts on demand
        // (packagekit, ssh behind ssh.socket) stops again when idle and is not missing then.
        let started_at_boot: BTreeSet<String> = lines(host, snapshot.join("systemd/enabled-units.txt"))
            .iter()
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect();
        Self {
            containers,
            services,
            ports: ports.into_iter().collect(),
            mounts: lines(host, snapshot.join("storage/mounts.txt")).iter().filter_map(real).collect(),
            units: lines(host, volatile.join("running-services.txt"))
                .iter()
                .filter_map(|l| l.split_whitespace().next().map(str::to_string))
                .filter(|unit| started_at_boot.contains(unit))
                .collect(),
            timers: lines(host, snapshot.join("cron/enabled-timers.txt"))
                .iter()
                .filter_map(|l| l.split_whitespace().next().map(str::to_string))
                .collect(),
            cron,
        }
    }

    pub fn load(host: &dyn Host, state: &StateDir) -> Option<Self> {
        serde_json::from_slice(&host.read(&Self::path(state)).ok()?).ok()
    }

    pub fn save(&self, host: &dyn Host, state: &StateDir) -> std::io::Result<()> {
        host.write(
            &Self::path(state),
            format!("{}\n", serde_json::to_string_pretty(self).unwrap_or_default()).as_bytes(),
            0o600,
        )
    }
}

const PENDING: &str = "verify.pending_boot";

/// The boot a verification is still owed for, if any.
pub fn pending(db: &Db) -> Option<String> {
    db.conn().query_row("SELECT value FROM settings WHERE key = ?1", [PENDING], |r| r.get(0)).ok()
}

/// Note that this boot must be verified (expectations are frozen until it has been).
pub fn expect_verification(db: &Db, boot: &str) {
    let _ = db.conn().execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (PENDING, boot),
    );
}

pub fn verified(db: &Db) {
    let _ = db.conn().execute("DELETE FROM settings WHERE key = ?1", [PENDING]);
}

/// After a snapshot: what it saw becomes what is expected — unless a boot still has to be verified
/// against what was there before it (a container that did not come back up must not vanish from the
/// expectations just because a snapshot ran first).
pub fn refresh(host: &dyn Host, state: &StateDir, db: &Db) {
    if pending(db).is_none() {
        let _ = Expect::from_state(host, state).save(host, state);
    }
}

/// One line of the checklist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub ok: bool,
    pub what: String,
    /// What to do about it, when it is not ok.
    pub hint: String,
}

fn good(what: impl Into<String>) -> Item {
    Item { ok: true, what: what.into(), hint: String::new() }
}

fn bad(what: impl Into<String>, hint: impl Into<String>) -> Item {
    Item { ok: false, what: what.into(), hint: hint.into() }
}

/// Group a list of missing things into one item, or say that none is missing.
fn all_present(what: &str, total: usize, missing: Vec<String>, hint: &str) -> Vec<Item> {
    if missing.is_empty() {
        return vec![good(format!("{what}: all {total}"))];
    }
    missing.into_iter().map(|m| bad(format!("{what}: {m}"), hint)).collect()
}

fn tool(host: &dyn Host, program: &str, args: &[&str]) -> Option<String> {
    host.run(&Cmd::new(program).args(args.iter().copied()).timeout(Duration::from_secs(30)))
        .ok()
        .map(|out| out.stdout_str().into_owned())
}

/// Compare the machine with the expectations and with what the configuration promises.
pub fn verify(host: &Arc<dyn Host>, config: &Arc<Config>, state: &StateDir, db: &Db) -> Result<Vec<Item>, Error> {
    let h = host.as_ref();
    let expect = Expect::load(h, state).unwrap_or_else(|| Expect::from_state(h, state));
    let mut items = Vec::new();
    let docker = Docker::new(Arc::clone(host));

    // ---- containers and swarm services
    if !expect.containers.is_empty() || !expect.services.is_empty() {
        let running: BTreeSet<String> = docker
            .containers(false)
            .unwrap_or_default()
            .iter()
            .flat_map(|c| {
                c["Names"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|n| n.trim_start_matches('/').to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        let missing = expect.containers.iter().filter(|c| !running.contains(*c)).cloned().collect();
        items.extend(all_present(
            "containers running",
            expect.containers.len(),
            missing,
            "docker ps -a; docker logs <name>; its stack: docker compose up -d",
        ));
        let services = docker.services_status().unwrap_or_default();
        let short: Vec<String> = expect
            .services
            .iter()
            .filter_map(|(name, wanted)| {
                let now = services
                    .iter()
                    .find(|s| s["Spec"]["Name"] == name.as_str())
                    .map(|s| s["ServiceStatus"]["RunningTasks"].as_u64().unwrap_or(0));
                match now {
                    Some(running) if running >= *wanted => None,
                    Some(running) => Some(format!("{name} ({running}/{wanted})")),
                    None => Some(format!("{name} (gone)")),
                }
            })
            .collect();
        if !expect.services.is_empty() {
            items.extend(all_present(
                "swarm services at their replicas",
                expect.services.len(),
                short,
                "docker service ps <name> --no-trunc",
            ));
        }
    }
    // ---- listening ports
    if !expect.ports.is_empty() {
        // `ss` prints: protocol, state, queues, local address, peer, process
        let now: BTreeSet<String> = tool(h, "ss", &["-tulpnH"])
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let fields: Vec<&str> = l.split_whitespace().collect();
                port(&format!("{} {}", fields.first()?, fields.get(4)?))
            })
            .collect();
        let missing = expect.ports.iter().filter(|p| !now.contains(*p)).cloned().collect();
        items.extend(all_present(
            "listening ports",
            expect.ports.len(),
            missing,
            "what served it is not up: ss -tulpn, then the container or unit behind the port",
        ));
    }
    // ---- mounts
    let mounted = crate::snapshot::mounts(h);
    let missing = expect.mounts.iter().filter(|m| !mounted.iter().any(|now| now.target == **m)).cloned().collect();
    items.extend(all_present(
        "mounts",
        expect.mounts.len(),
        missing,
        "check /etc/fstab and `mount -a`; is the disk attached?",
    ));
    for mount in &config.checks.storage.writable {
        let probe = Path::new(mount).join(".moi-verify");
        let writable = h.write(&probe, b"", 0o600).is_ok();
        let _ = h.remove_file(&probe);
        items.push(if writable {
            good(format!("{mount} is writable"))
        } else {
            bad(format!("{mount} is not writable"), "mounted read-only or not at all: findmnt, dmesg")
        });
    }
    // ---- systemd
    let running = tool(
        h,
        "systemctl",
        &["list-units", "--type=service", "--state=running", "--no-legend", "--no-pager", "--plain"],
    );
    if let Some(running) = &running {
        let now: BTreeSet<&str> = running.lines().filter_map(|l| l.split_whitespace().next()).collect();
        let missing = expect.units.iter().filter(|u| !now.contains(u.as_str())).cloned().collect();
        items.extend(all_present(
            "services running",
            expect.units.len(),
            missing,
            "systemctl status <unit>; journalctl -u <unit> -b",
        ));
    }
    if let Some(timers) =
        tool(h, "systemctl", &["list-unit-files", "--type=timer", "--state=enabled", "--no-legend", "--no-pager"])
    {
        let now: BTreeSet<&str> = timers.lines().filter_map(|l| l.split_whitespace().next()).collect();
        let missing = expect.timers.iter().filter(|t| !now.contains(t.as_str())).cloned().collect();
        items.extend(all_present("timers enabled", expect.timers.len(), missing, "systemctl enable --now <timer>"));
    }
    if let Some(failed) = tool(h, "systemctl", &["list-units", "--failed", "--no-legend", "--no-pager", "--plain"]) {
        let units: Vec<&str> = failed.lines().filter_map(|l| l.split_whitespace().next()).collect();
        items.push(if units.is_empty() {
            good("no failed units")
        } else {
            bad(format!("failed units: {}", units.join(", ")), "systemctl status <unit>; journalctl -u <unit> -b")
        });
    }
    // ---- cron
    let missing = expect
        .cron
        .iter()
        .filter(|entry| match entry.strip_prefix("crontab of ") {
            Some(user) => {
                !["/var/spool/cron/crontabs", "/var/spool/cron"].iter().any(|dir| h.exists(&Path::new(dir).join(user)))
            }
            None => !h.exists(Path::new(entry)),
        })
        .cloned()
        .collect();
    items.extend(all_present(
        "cron entries",
        expect.cron.len(),
        missing,
        "restore it from the state directory (snapshot/cron, snapshot/files/etc/cron.d)",
    ));
    // ---- the daemon and its schedules
    match crate::daemon::ask(h, &serde_json::json!({ "cmd": "status" })) {
        Some(status) => items
            .push(good(format!("the moi daemon is running ({} jobs)", status["jobs"].as_array().map_or(0, Vec::len)))),
        None => items.push(bad("the moi daemon is not running", "systemctl status moi; systemctl start moi")),
    }
    // ---- firewall
    if config.firewall.manage {
        for family in crate::firewall::Family::ALL {
            let now = crate::firewall::state(h, config, db, family)?;
            let name = family.name();
            items.push(match now {
                crate::firewall::State::InSync => {
                    good(format!("firewall ({name}): the managed chains are what moi applied"))
                }
                crate::firewall::State::Unavailable => {
                    good(format!("firewall ({name}): not available on this machine"))
                }
                other => {
                    bad(format!("firewall ({name}): {}", other.describe()), "moi firewall diff; moi firewall apply")
                }
            });
        }
    }
    let required = &config.checks.edge.firewall_required;
    if config.checks.edge.enabled && !required.is_empty() {
        let rules = tool(h, "iptables", &["-S", "DOCKER-USER"]).unwrap_or_default();
        let missing: Vec<String> = required.iter().filter(|text| !rules.contains(text.as_str())).cloned().collect();
        items.extend(all_present(
            "required firewall rules",
            required.len(),
            missing,
            "iptables -S DOCKER-USER; moi firewall apply",
        ));
    }
    // ---- moi's own checks, and whether Kuma takes their results
    let all = checks::enabled(config);
    let mut pushes = (0, Vec::new());
    for check in &all {
        let mut result = checks::run(host, config, db, check, Mode::Dry)?;
        items.push(match result.status {
            Status::Up => good(format!("check {}: {}", result.name, result.message)),
            Status::Down => {
                bad(format!("check {}: {}", result.name, result.message), format!("moi check {}", result.name))
            }
        });
        if result.monitor.is_some() && config.monitors.kuma.enabled {
            result.pushed = Some(checks::push(host, config, db, &result));
            match result.pushed.as_deref() {
                Some("ok") => pushes.0 += 1,
                Some(why) => pushes.1.push(format!("{}: {why}", result.name)),
                None => {}
            }
        }
    }
    if pushes.0 + pushes.1.len() > 0 {
        items.push(if pushes.1.is_empty() {
            good(format!("Uptime Kuma accepted {} results", pushes.0))
        } else {
            bad(
                format!("Uptime Kuma did not take every result: {}", pushes.1.join("; ")),
                "moi monitors sync --dry-run; is Kuma up?",
            )
        });
    }
    if config.monitors.healthchecks.expect_watchdog {
        let watchdog = crate::monitors::local_inventory(h, config, &docker)
            .ok()
            .is_some_and(|inventory| inventory.monitors.iter().any(|m| m.is_watchdog() && m.active));
        items.push(if watchdog {
            good("the healthchecks.io watchdog monitor is active")
        } else {
            bad(
                "no active healthchecks.io watchdog monitor",
                "in Uptime Kuma: resume or recreate the monitor that pings hc-ping.com",
            )
        });
    }
    // ---- backups could run
    let backup = &config.backup;
    if backup.enabled {
        if let Some(mount) = &backup.require_mount {
            let there = mounted.iter().any(|m| m.target == *mount);
            items.push(if there {
                good(format!("backups: {mount} is mounted"))
            } else {
                bad(format!("backups: {mount} is not mounted"), "the next backup would refuse to run")
            });
        }
        if backup.offsite.enabled {
            let reachable = crate::backup::offsite::Restic::new(host, &backup.offsite).and_then(|restic| {
                restic.run(&["cat", "config"], Some(Duration::from_secs(120))).map_err(|e| Error::host("restic", e))
            });
            items.push(match reachable {
                Ok(out) if out.status == 0 => good("backups: the off-site repository answers"),
                Ok(out) => bad(
                    format!(
                        "backups: the off-site repository does not answer ({})",
                        out.stderr_str().lines().last().unwrap_or("no reason given").trim()
                    ),
                    "check the repository URL, password and keys in secrets.toml",
                ),
                Err(e) => bad(format!("backups: restic cannot run: {e}"), "install restic"),
            });
            let guard = &backup.offsite.r2_guard;
            if let (true, Some(account), Some(token)) = (guard.enabled, &guard.account_id, &guard.api_token) {
                items.push(match crate::backup::offsite::r2_usage(host, account.expose(), token.expose()) {
                    Ok(_) => good("backups: R2 usage can be read"),
                    Err(why) => bad(
                        format!("backups: {why}"),
                        "the next upload would be refused: check the Cloudflare account id and token",
                    ),
                });
            }
        }
    }
    Ok(items)
}

/// The checklist as text.
pub fn render(items: &[Item]) -> String {
    let mut out = String::new();
    for item in items {
        if item.ok {
            out.push_str(&format!("✓ {}\n", item.what));
        } else {
            out.push_str(&format!("✗ {}\n    → {}\n", item.what, item.hint));
        }
    }
    let failed = items.iter().filter(|i| !i.ok).count();
    out.push_str(&if failed == 0 {
        format!("all {} checks passed\n", items.len())
    } else {
        format!("{failed} of {} checks FAILED\n", items.len())
    });
    out
}
