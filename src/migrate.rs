//! Moving a server from its legacy scripts, cron jobs and units to moi: what replaces what
//! ([`plan`]), and — in the submodules to come — running in the shadow, comparing, switching and
//! going back.
//!
//! Spec: docs/spec/11-migration.md.
//!
//! Nothing here knows a particular server. What is legacy is recognized by what it *does*: a script
//! that pushes to the monitor a moi check reports to, a unit named like a heal rule, a unit that
//! starts compose stacks when `boot.compose_up` does, a unit that loads firewall rules when moi
//! manages the firewall. What cannot be recognized is named in `migrate.remove`.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::config::Config;
use crate::host::Host;
use crate::state::StateDir;

/// What kind of thing a legacy piece is — which decides how it is switched off and put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A file in `/etc/cron.d`: moved away to stop it.
    Cron,
    /// A systemd unit: disabled and stopped.
    Unit,
    /// A script or a configuration file of the legacy setup.
    File,
    /// What the legacy scripts remembered between runs.
    State,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Cron => "cron",
            Kind::Unit => "unit",
            Kind::File => "file",
            Kind::State => "state",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Legacy {
    pub kind: Kind,
    pub path: String,
}

/// One thing that moves from legacy to moi as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    /// `resources`, `backup`, `heal:<rule>`, `boot`, `firewall`, `stale`, `serverstate`.
    pub name: String,
    /// The part of moi that takes over.
    pub by: String,
    pub legacy: Vec<Legacy>,
    /// What stays the same across the switch.
    pub keeps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub items: Vec<Item>,
    /// Things that look like the legacy setup's but that moi does not replace, each with why.
    pub untouched: Vec<(String, String)>,
}

/// A custom unit file with what it runs.
struct Unit {
    path: String,
    name: String,
    exec: Vec<String>,
}

impl Unit {
    fn stem(&self) -> &str {
        self.name.rsplit_once('.').map_or(self.name.as_str(), |(stem, _)| stem)
    }

    /// The program a unit starts, if it is a file of its own (not `/usr/bin/docker`).
    fn script(&self, host: &dyn Host) -> Option<String> {
        let program = self.exec.first()?.split_whitespace().next()?.trim_start_matches(['-', '@', '+', '!']);
        (!program.starts_with("/usr/bin/") && !program.starts_with("/bin/") && host.exists(Path::new(program)))
            .then(|| program.to_string())
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `NAME=/path` assignments in a shell script: where it keeps what it remembers, and the files it
/// reads its settings from.
fn assigned_paths(script: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|line| {
            let (name, value) = line.trim().split_once('=')?;
            let named =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            let value = value.split_whitespace().next()?.trim_matches(['"', '\'']);
            (named && value.starts_with('/')).then(|| value.to_string())
        })
        .collect()
}

/// Where a legacy script keeps its own state: its directory under `/var/lib`, a stamp in `/var/tmp`,
/// a settings file under `/root`. Logs stay (other tools read them); so does everything of Docker and moi.
fn state_of(host: &dyn Host, script: &str) -> Vec<Legacy> {
    let mut out = Vec::new();
    for path in assigned_paths(script) {
        let kept = if let Some(rest) = path.strip_prefix("/var/lib/") {
            let top = rest.split('/').next().unwrap_or_default();
            (!["docker", "moi", "incus", ""].contains(&top)).then(|| format!("/var/lib/{top}"))
        } else if path.starts_with("/var/tmp/") || path.starts_with("/root/") {
            Some(path.clone())
        } else {
            None
        };
        if let Some(path) = kept.filter(|p| host.exists(Path::new(p))) {
            out.push(Legacy { kind: Kind::State, path });
        }
    }
    out
}

/// What will be replaced by what. Reads the machine and the last snapshot; changes nothing.
pub fn plan(host: &dyn Host, config: &Config, state: &StateDir) -> Plan {
    let read = |path: &str| host.read_to_string(Path::new(path)).unwrap_or_default();
    let lines = |file: &str| -> Vec<String> {
        host.read_to_string(&state.snapshot().join(file)).unwrap_or_default().lines().map(str::to_string).collect()
    };
    let integrations: Vec<Value> = host
        .read(&state.snapshot().join("monitoring/integrations.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let units: Vec<Unit> = lines("systemd/custom-units.txt")
        .into_iter()
        .filter(|path| !path.ends_with(".conf") && !path.ends_with("/moi.service"))
        .map(|path| {
            let exec =
                read(&path).lines().filter_map(|l| l.trim().strip_prefix("ExecStart=")).map(str::to_string).collect();
            Unit { name: file_name(&path).to_string(), path, exec }
        })
        .collect();
    let script_dirs: Vec<&str> = config.capture.script_dirs.iter().map(String::as_str).collect();
    let in_script_dir = |path: &str| script_dirs.iter().any(|dir| crate::backup::under(path, dir));

    let mut items: Vec<Item> = Vec::new();
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    let add = |items: &mut Vec<Item>,
               claimed: &mut BTreeSet<String>,
               name: &str,
               by: String,
               mut legacy: Vec<Legacy>,
               keeps: Vec<String>| {
        legacy.retain(|l| claimed.insert(l.path.clone()));
        legacy.sort();
        if !legacy.is_empty() {
            items.push(Item { name: name.to_string(), by, legacy, keeps });
        }
    };
    // every unit of a family: `x.service` with its `x.timer` and `x.path`
    let family = |stem: &str| -> Vec<Legacy> {
        units.iter().filter(|u| u.stem() == stem).map(|u| Legacy { kind: Kind::Unit, path: u.path.clone() }).collect()
    };
    let with_script = |unit: &Unit, legacy: &mut Vec<Legacy>| -> String {
        let script = unit.script(host).unwrap_or_default();
        if !script.is_empty() {
            legacy.push(Legacy { kind: Kind::File, path: script.clone() });
        }
        script
    };

    // ---- heal rules: a unit named like the rule did its job before
    for rule in &config.heal {
        let mut legacy = family(&rule.name);
        for unit in units.iter().filter(|u| u.stem() == rule.name) {
            with_script(unit, &mut legacy);
        }
        add(
            &mut items,
            &mut claimed,
            &format!("heal:{}", rule.name),
            format!("heal rule {}", rule.name),
            legacy,
            Vec::new(),
        );
    }

    // ---- stacks started at boot
    if !config.boot.compose_up.is_empty() {
        let mut legacy = Vec::new();
        for unit in &units {
            let script = unit.script(host).unwrap_or_default();
            let starts = |text: &str| text.contains("compose up") || text.contains("docker-compose up");
            if unit.exec.iter().any(|e| starts(e)) || (!script.is_empty() && starts(&read(&script))) {
                legacy.extend(family(unit.stem()));
                with_script(unit, &mut legacy);
            }
        }
        add(&mut items, &mut claimed, "boot", "boot.compose_up".to_string(), legacy, Vec::new());
    }

    // ---- the firewall
    if config.firewall.manage {
        let mut legacy = Vec::new();
        let mut keeps = Vec::new();
        for unit in &units {
            let script = unit.script(host).unwrap_or_default();
            // loading rules, not looking at them (a health check may list the chains)
            let loads = |text: &str| {
                ["iptables-restore", "ip6tables-restore", "iptables -A", "iptables -I", "nft -f"]
                    .iter()
                    .any(|w| text.contains(w))
            };
            if unit.exec.iter().any(|e| loads(e)) || (!script.is_empty() && loads(&read(&script))) {
                legacy.extend(family(unit.stem()));
                with_script(unit, &mut legacy);
                // the rules files it was given
                for argument in unit.exec.iter().flat_map(|e| e.split_whitespace().skip(1)) {
                    if argument.starts_with('/') && host.exists(Path::new(argument)) {
                        legacy.push(Legacy { kind: Kind::File, path: argument.to_string() });
                    }
                }
            }
        }
        if !legacy.is_empty() {
            keeps.push(format!("the rules, now in {} and {}", config.firewall.rules_v4, config.firewall.rules_v6));
        }
        add(&mut items, &mut claimed, "firewall", "firewall".to_string(), legacy, keeps);
    }

    // ---- checks and the backup: the scripts that feed their monitors
    let feeders = crate::monitors::feeders(host, config, state);
    let checks = crate::checks::enabled(config);
    for feeder in &feeders {
        let name = feeder.owner.strip_prefix("check:").unwrap_or(&feeder.owner).to_string();
        let mut legacy = vec![Legacy { kind: Kind::File, path: feeder.file.clone() }];
        let text = read(&feeder.file);
        // scripts of the same directory that only this one calls go with it
        let dir = feeder.file.rsplit_once('/').map_or("", |(dir, _)| dir);
        let mut scripts = vec![text.clone()];
        for other in host.read_dir(Path::new(dir)).unwrap_or_default() {
            let path = format!("{dir}/{other}");
            let called_elsewhere = integrations.iter().any(|i| i["file"] == path.as_str())
                || units.iter().any(|u| u.exec.iter().any(|e| e.contains(&path)));
            if path != feeder.file
                && text.contains(&path)
                && !called_elsewhere
                && host.metadata(Path::new(&path)).is_ok_and(|m| m.kind == crate::host::FileKind::File)
            {
                legacy.push(Legacy { kind: Kind::File, path: path.clone() });
                scripts.push(read(&path));
            }
        }
        for script in &scripts {
            legacy.extend(state_of(host, script));
        }
        // what starts it
        let callers = integrations
            .iter()
            .find(|i| i["file"] == feeder.file.as_str())
            .map(|i| i["called_by"].clone())
            .unwrap_or_default();
        for caller in callers.as_array().into_iter().flatten().filter_map(Value::as_str) {
            let path = caller.split(": ").next().unwrap_or_default();
            if path.starts_with("/etc/cron.d/") {
                legacy.push(Legacy { kind: Kind::Cron, path: path.to_string() });
            } else if let Some(unit) = units.iter().find(|u| u.path == path) {
                legacy.extend(family(unit.stem()));
            }
        }
        let (by, mut keeps) = if name == "backup" {
            let mut keeps = Vec::new();
            if let Some(file) = &config.backup.log_file {
                keeps.push(format!("log lines in {file}"));
            }
            if config.backup.offsite.enabled {
                keeps.push("the off-site repository, its password and its snapshots".to_string());
            }
            (if config.backup.offsite.enabled { "backup + backup.offsite" } else { "backup" }.to_string(), keeps)
        } else {
            let mut by = format!("checks.{name}");
            let mut keeps = Vec::new();
            if name == "services" {
                if config.checks.edge.enabled {
                    by.push_str(" + checks.edge");
                }
                let triggered = config
                    .heal
                    .iter()
                    .filter(|h| !claimed.iter().any(|c| file_name(c).starts_with(&format!("{}.", h.name))))
                    .count();
                if triggered > 0 {
                    by.push_str(&format!(" + {triggered} heal rule{}", if triggered == 1 { "" } else { "s" }));
                }
            }
            if let Some(check) = checks.iter().find(|c| c.name() == name) {
                keeps.push(format!("journal tag {}", check.journal_tag(config)));
            }
            (by, keeps)
        };
        let monitor = if name == "backup" {
            config.checks.backup.monitor.clone()
        } else {
            checks.iter().find(|c| c.name() == name).and_then(|c| c.monitor(config))
        };
        let tail: String = feeder.token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        keeps.insert(0, format!("push token …{tail}{}", monitor.map(|m| format!(" of \"{m}\"")).unwrap_or_default()));
        add(&mut items, &mut claimed, &name, by, legacy, keeps);
    }

    // ---- copies nothing calls, and what the owner named
    let mut stale: Vec<Legacy> = integrations
        .iter()
        .filter(|i| i["stale_copy"] == true)
        .filter_map(|i| i["file"].as_str())
        .map(|path| Legacy { kind: Kind::File, path: path.to_string() })
        .collect();
    stale.extend(
        config
            .migrate
            .remove
            .iter()
            .filter(|p| host.exists(Path::new(p)))
            .map(|path| Legacy { kind: Kind::File, path: path.clone() }),
    );
    add(
        &mut items,
        &mut claimed,
        "stale",
        "nothing (stale copies and what `migrate.remove` names)".to_string(),
        stale,
        Vec::new(),
    );

    // ---- the prototype this tool grew out of
    let serverstate: Vec<Legacy> = [
        (Kind::State, "/var/lib/serverstate"),
        (Kind::File, "/etc/serverstate.conf"),
        (Kind::File, "/usr/local/lib/serverstate"),
        (Kind::File, "/usr/local/sbin/serverstate"),
        (Kind::File, "/usr/local/bin/serverstate"),
    ]
    .into_iter()
    .filter(|(_, path)| host.exists(Path::new(path)))
    .map(|(kind, path)| Legacy { kind, path: path.to_string() })
    .chain(
        units
            .iter()
            .filter(|u| u.name.starts_with("serverstate-"))
            .map(|u| Legacy { kind: Kind::Unit, path: u.path.clone() }),
    )
    .collect();
    let keeps = vec!["its history and NOTES.md, imported into moi's state".to_string()];
    add(
        &mut items,
        &mut claimed,
        "serverstate",
        "moi's state (snapshot, history, context)".to_string(),
        serverstate,
        keeps,
    );

    // ---- what looks like the legacy setup's and stays
    let mut untouched: Vec<(String, String)> = Vec::new();
    for unit in units.iter().filter(|u| !claimed.contains(&u.path)) {
        untouched.push((unit.path.clone(), "a unit moi has no replacement for".to_string()));
    }
    for name in host.read_dir(Path::new("/etc/cron.d")).unwrap_or_default() {
        let path = format!("/etc/cron.d/{name}");
        let runs_script = read(&path).split_whitespace().any(|word| in_script_dir(word));
        if !claimed.contains(&path) && runs_script {
            untouched.push((path, "a cron job moi has no replacement for".to_string()));
        }
    }
    for integration in integrations.iter().filter(|i| i["kind"] == "uptime-kuma-push") {
        if let Some(container) = integration["container"].as_str() {
            untouched.push((format!("container {container}"), "pushes to a monitor of its own".to_string()));
        }
    }
    for dir in &script_dirs {
        for name in host.read_dir(Path::new(dir)).unwrap_or_default() {
            let path = format!("{}/{name}", dir.trim_end_matches('/'));
            let is_file = host.metadata(Path::new(&path)).is_ok_and(|m| m.kind == crate::host::FileKind::File);
            if is_file && !claimed.contains(&path) && lines("scripts/referenced.txt").contains(&path) {
                untouched.push((path, "a script something still calls".to_string()));
            }
        }
    }
    untouched.sort();
    untouched.dedup();
    Plan { items, untouched }
}

/// The plan as people read it.
pub fn render(plan: &Plan) -> String {
    let mut out = String::new();
    if plan.items.is_empty() {
        out.push_str("nothing to migrate: no legacy script, cron job or unit does what moi is configured to do\n");
    }
    for item in &plan.items {
        out.push_str(&format!("{}  →  {}\n", item.name, item.by));
        for legacy in &item.legacy {
            out.push_str(&format!("  {:<6} {}\n", legacy.kind.as_str(), legacy.path));
        }
        for keep in &item.keeps {
            out.push_str(&format!("  keeps  {keep}\n"));
        }
        out.push('\n');
    }
    if !plan.untouched.is_empty() {
        out.push_str("untouched\n");
        for (what, why) in &plan.untouched {
            out.push_str(&format!("  {what} — {why}\n"));
        }
    }
    out
}
