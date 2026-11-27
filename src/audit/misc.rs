//! § 7 secrets written into scripts, § 8 programs listening outside Docker, § 9 monitoring integrity.

use std::collections::BTreeSet;

use serde_json::Value;

use super::Section;
use crate::config::Config;
use crate::redact::mask_token;
use crate::snapshot::View;

/// `[export ]NAME=value` at the start of a line, NAME containing KEY/TOKEN/PASS/SECRET, value a literal
/// of 16+ token characters.
pub(crate) fn hardcoded_secret(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = line.strip_prefix("export").map_or(line, str::trim_start);
    let (name, value) = line.split_once('=')?;
    let named = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && ["KEY", "TOKEN", "PASS", "SECRET"].iter().any(|k| name.contains(k));
    let literal = value.trim_start_matches(['"', '\'']);
    let length = literal.chars().take_while(|c| c.is_ascii_alphanumeric() || "+/_-".contains(*c)).count();
    (named && length >= 16).then_some(name)
}

pub(super) fn secrets(view: &View<'_>) -> Section {
    let mut s = Section::new("secrets", "Secrets written into scripts");
    let mut found = BTreeSet::new();
    for script in view.lines("scripts/index.txt") {
        for line in view.file(&script).unwrap_or_default().lines() {
            if let Some(name) = hardcoded_secret(line) {
                found.insert(format!("`{script}`: {name}"));
            }
        }
    }
    for item in found {
        s.review(format!("{item} — captured and restored as it is; move it to `secrets.toml` (a migrated check reads `secret:` references instead)"));
    }
    s.or_nothing("none found")
}

pub(super) fn listeners(view: &View<'_>) -> Section {
    let mut s = Section::new("listeners", "Programs listening on the host outside Docker");
    const EXPECTED: [&str; 6] = ["docker-proxy", "systemd-resolve", "sshd", "systemd", "chronyd", "dockerd"];
    let containers = view.containers();
    let units = view.custom_units();
    let mut seen = BTreeSet::new();
    for line in view.volatile_lines("listening.txt") {
        let mut fields = line.splitn(3, ' ');
        let (Some(proto), Some(address), Some(users)) = (fields.next(), fields.next(), fields.next()) else { continue };
        let programs: BTreeSet<&str> = users.split('"').skip(1).step_by(2).collect();
        if programs.is_empty() || programs.iter().all(|p| EXPECTED.contains(p)) {
            continue;
        }
        for program in programs.into_iter().filter(|p| !EXPECTED.contains(p)) {
            let lower = program.to_lowercase();
            // a host-network container is "outside Docker" to the kernel, but Docker brings it back
            let container = containers.iter().find(|c| {
                c["host_config"]["NetworkMode"] == "host"
                    && [c["name"].as_str(), c["image"].as_str()].iter().flatten().any(|v| {
                        let v = v.to_lowercase();
                        v.contains(&lower)
                            || lower.contains(v.rsplit('/').next().unwrap_or(&v).split(':').next().unwrap_or(&v))
                    })
            });
            let unit = units.iter().find(|u| view.file(u).is_some_and(|t| t.to_lowercase().contains(&lower)));
            let provider = match (container, unit) {
                (Some(c), _) => format!("container {} (host network)", c["name"].as_str().unwrap_or_default()),
                (None, Some(unit)) => format!("unit {}", unit.rsplit('/').next().unwrap_or(unit)),
                (None, None) => String::new(),
            };
            if !seen.insert(format!("{proto} {address} {program}")) {
                continue;
            }
            if provider.is_empty() {
                s.review(format!("{proto} {address} — {program}: make sure a package, unit or script the rebuild recreates provides it"));
            } else {
                s.ok(format!("{proto} {address} — {program}: provided by {provider}"));
            }
        }
    }
    s.or_nothing("only the usual (sshd, docker-proxy, systemd)")
}

pub(super) fn monitoring(view: &View<'_>, config: &Config) -> Section {
    let mut s = Section::new("monitoring", "Monitoring integrity");
    let kuma = view.json("monitoring/kuma-monitors.json");
    let monitors: Vec<&Value> = kuma["monitors"].as_array().into_iter().flatten().collect();
    let integrations = view.list("monitoring/integrations.json");
    let last = view.volatile("kuma-last.json");
    let last: Value = serde_json::from_str(&last).unwrap_or(Value::Null);
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();

    let pushes: Vec<&Value> = integrations.iter().filter(|i| i["kind"] == "uptime-kuma-push").collect();
    let tokens: BTreeSet<String> = monitors.iter().map(|m| text(&m["push_token"])).filter(|t| !t.is_empty()).collect();
    let feeder_name = |i: &Value| {
        if i["file"].is_string() { text(&i["file"]) } else { format!("container {}", text(&i["container"])) }
    };

    if !monitors.is_empty() {
        for m in monitors.iter().filter(|m| m["type"] == "push") {
            let token = text(&m["push_token"]);
            if !pushes.iter().any(|i| i["stale_copy"] != true && text(&i["token"]) == token) {
                s.review(format!(
                    "push monitor **{}** ({}): nothing captured on this server pushes to it",
                    text(&m["name"]),
                    mask_token(&token)
                ));
            }
        }
        for i in &pushes {
            let token = text(&i["token"]);
            if i["stale_copy"] == true {
                let known = if tokens.contains(&token) { "a live monitor's token" } else { "a token no monitor has" };
                s.review(format!(
                    "`{}` is a stale copy holding {known} ({}): delete it",
                    text(&i["file"]),
                    mask_token(&token)
                ));
            } else if !tokens.contains(&token) {
                s.gap(format!(
                    "{} pushes to {} with a token no monitor has ({}): its reports go nowhere",
                    feeder_name(i),
                    text(&i["base"]),
                    mask_token(&token)
                ));
            }
        }
        for m in &monitors {
            let name = text(&m["name"]);
            let message = last[&name]["msg"].as_str().unwrap_or_default();
            if message.contains("[object Object]") || message.contains("undefined") {
                s.review(format!("monitor **{name}** last received a malformed message (`{message}`): whatever feeds it sends an object, not text"));
            }
        }
    } else if !pushes.is_empty() {
        s.info(format!(
            "{} push integration(s) found, but Uptime Kuma's monitor list is not readable from this server",
            pushes.len()
        ));
    }

    // moi's own checks and where their results go
    let checks = &config.checks;
    let mut wanted: Vec<(&str, &Option<String>, bool)> = vec![
        ("resources", &checks.resources.common.monitor, checks.resources.common.enabled),
        ("storage", &checks.storage.common.monitor, checks.storage.common.enabled),
        ("services", &checks.services.common.monitor, checks.services.common.enabled),
        ("backup", &checks.backup.monitor, config.backup.enabled),
    ];
    wanted.extend(checks.custom.iter().map(|c| (c.name.as_str(), &c.common.monitor, c.common.enabled)));
    for (check, monitor, _) in wanted.into_iter().filter(|(_, _, enabled)| *enabled) {
        match monitor {
            None => s.review(format!("check **{check}** has no `monitor`: its result is only visible in `moi check`")),
            Some(name) if !monitors.is_empty() && !monitors.iter().any(|m| m["name"] == name.as_str()) => {
                s.review(format!("check **{check}** reports to monitor \"{name}\", which Uptime Kuma does not have yet (`moi monitors sync` creates it)"));
            }
            Some(_) => {}
        }
    }
    s.or_nothing("nothing found")
}
