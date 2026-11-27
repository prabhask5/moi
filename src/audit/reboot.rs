//! § 1 — Would it survive a reboot?

use std::collections::{BTreeMap, BTreeSet};

use super::Section;
use crate::config::Config;
use crate::snapshot::View;

/// Custom rules worth keeping: not Docker's own chains, not bridge plumbing, not ban entries.
pub(crate) fn custom_rules(iptables_save: &str) -> BTreeSet<String> {
    let mut table = "";
    let mut out = BTreeSet::new();
    for line in iptables_save.lines() {
        if let Some(name) = line.strip_prefix('*') {
            table = name;
        }
        let stable = crate::snapshot::stable_rules(line);
        if line.starts_with("-A") && !stable.is_empty() {
            out.insert(format!("{table} {}", stable.trim()));
        }
    }
    out
}

/// `/opt/services/**` covers everything below; `*` is a wildcard within the pattern.
fn glob_covers(pattern: &str, path: &str) -> bool {
    match pattern.strip_suffix("/**") {
        Some(base) => crate::backup::under(path, base),
        None => crate::snapshot::wildcard(pattern, path),
    }
}

pub(super) fn section(view: &View<'_>, config: &Config) -> Section {
    let mut s = Section::new("reboot", "Would it survive a reboot?");
    let enabled: BTreeSet<String> = view.lines("systemd/enabled-units.txt").into_iter().collect();
    let custom_units = view.custom_units();
    let unit_name = |path: &str| path.rsplit('/').next().unwrap_or(path).to_string();
    // unit text plus the text of what its Exec lines run
    let unit_text = |path: &str| -> String {
        let mut text = view.file(path).unwrap_or_default();
        let programs: Vec<String> = text
            .lines()
            .filter_map(|l| l.trim().split_once('='))
            .filter(|(key, _)| key.starts_with("Exec"))
            .filter_map(|(_, cmd)| {
                cmd.trim_start_matches(['-', '@', '+', '!']).split_whitespace().next().map(str::to_string)
            })
            .collect();
        for program in programs {
            text.push_str(&view.file(&program).unwrap_or_default());
        }
        text
    };
    let enabled_custom: Vec<(String, String)> =
        custom_units.iter().filter(|u| enabled.contains(&unit_name(u))).map(|u| (unit_name(u), unit_text(u))).collect();

    // containers without a restart policy
    let states = view.container_states();
    let projects = view.compose_projects();
    for c in view.containers() {
        let name = c["name"].as_str().unwrap_or_default();
        let policy = c["host_config"]["RestartPolicy"]["Name"].as_str().unwrap_or("no");
        if states.get(name).map(String::as_str) != Some("running") || !(policy.is_empty() || policy == "no") {
            continue;
        }
        let workdir = projects
            .iter()
            .find(|p| p["containers"].as_array().is_some_and(|cs| cs.iter().any(|x| x == name)))
            .and_then(|p| p["workdir"].as_str())
            .unwrap_or_default();
        let by_moi = !workdir.is_empty() && config.boot.compose_up.iter().any(|g| glob_covers(g, workdir));
        // a unit starts the stack if it runs `compose … up` and names the stack's directory, or loops
        // over its parent (`for d in /opt/services/*/`)
        let parent_glob = workdir.rsplit_once('/').map(|(parent, _)| format!("{parent}/*")).unwrap_or_default();
        let names_dir = |text: &str| {
            text.match_indices(workdir).any(|(i, _)| {
                !text[i + workdir.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '-' || c == '_')
            }) || (parent_glob.len() > 2 && text.contains(&parent_glob))
        };
        let by_unit = enabled_custom.iter().find(|(_, text)| {
            !workdir.is_empty() && text.contains("compose") && text.contains(" up") && names_dir(text)
        });
        if by_moi {
            s.ok(format!("container **{name}** has no restart policy, but its stack `{workdir}` is started at boot by moi (`boot.compose_up`)"));
        } else if let Some((unit, _)) = by_unit {
            s.ok(format!(
                "container **{name}** has no restart policy, but its stack `{workdir}` is started at boot by {unit}"
            ));
        } else {
            s.gap(format!(
                "container **{name}** is running with restart policy \"no\": it will not come back after a reboot"
            ));
        }
    }

    // running services whose unit is disabled
    let unit_states: BTreeMap<String, String> = view
        .lines("systemd/unit-file-states.txt")
        .iter()
        .filter_map(|l| l.split_once(' '))
        .map(|(u, st)| (u.to_string(), st.trim().to_string()))
        .collect();
    let triggered: BTreeMap<String, String> = view
        .lines("systemd/triggered-by.txt")
        .iter()
        .filter_map(|l| l.split_once(' '))
        .map(|(u, by)| (u.to_string(), by.trim().to_string()))
        .collect();
    for line in view.volatile_lines("running-services.txt") {
        let Some(unit) = line.split_whitespace().next() else { continue };
        if unit_states.get(unit).map(String::as_str) != Some("disabled") {
            continue;
        }
        let pulled_in = enabled_custom.iter().find(|(_, text)| {
            text.lines().any(|l| {
                (l.trim_start().starts_with("Wants=") || l.trim_start().starts_with("Requires=")) && l.contains(unit)
            })
        });
        if let Some(by) = triggered.get(unit) {
            s.ok(format!("service **{unit}** is disabled but started on demand by {by}"));
        } else if let Some((by, _)) = pulled_in {
            s.ok(format!("service **{unit}** is disabled but pulled in by {by}"));
        } else {
            s.gap(format!("service **{unit}** is running but disabled (started by hand?): it will not start on boot"));
        }
    }

    // mounted filesystems that fstab does not know
    let fstab: BTreeSet<String> = view
        .file("/etc/fstab")
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .collect();
    for line in view.lines("storage/mounts.txt") {
        let f: Vec<&str> = line.split_whitespace().collect();
        let [target, source, fstype, ..] = f.as_slice() else { continue };
        let system = *target == "/" || ["/boot", "/snap", "/run"].iter().any(|p| target.starts_with(p));
        if fstab.contains(*target)
            || system
            || !source.starts_with("/dev/")
            || ["zfs", "fuse.rclone", "nfsd"].contains(fstype)
        {
            continue;
        }
        if source.starts_with("/dev/loop") {
            let owner = if target.starts_with("/var/lib/incus") {
                Some("incus-startup.service")
            } else if target.contains("lxd") {
                Some("snap.lxd.daemon.service")
            } else {
                None
            };
            match owner.filter(|o| enabled.contains(*o)) {
                Some(owner) => s.ok(format!("`{target}` (a loop image) is mounted at boot by {owner}")),
                None => s.review(format!(
                    "**{target}** is a loop-device mount ({fstype}): make sure the service that mounts it is enabled"
                )),
            }
        } else {
            s.gap(format!("**{target}** ({source}, {fstype}) is mounted but not in /etc/fstab: it will not be mounted after a reboot"));
        }
    }

    // firewall rules that live only in the kernel
    let live = custom_rules(&view.volatile("iptables.raw"));
    let managers = view.lines("firewall/managers.txt");
    let custom: Vec<&String> = live
        .iter()
        .filter(|r| [" -A INPUT ", " -A DOCKER-USER ", " -A FORWARD "].iter().any(|chain| r.contains(chain)))
        .filter(|r| !r.ends_with("DOCKER-USER -j RETURN") && !r.contains("ufw") && !r.contains(" -j DOCKER-USER"))
        .collect();
    let saved_text = view.file("/etc/iptables/rules.v4");
    if config.firewall.manage && !custom.is_empty() {
        s.ok(format!(
            "{} custom firewall rules: chains {} are applied at boot by moi (`firewall.manage`)",
            custom.len(),
            config.firewall.chains.join(", ")
        ));
    } else if let Some(saved) = saved_text.filter(|_| !live.is_empty()) {
        let saved = custom_rules(&saved);
        let internal = |r: &str| {
            ["ufw", "ufw6", "f2b", "cali", "KUBE", "ts-"]
                .iter()
                .any(|p| r.split_whitespace().nth(2).is_some_and(|chain| chain.starts_with(p)))
        };
        let missing: Vec<String> = live.difference(&saved).filter(|r| !internal(r)).cloned().collect();
        if missing.is_empty() {
            s.ok("live iptables rules match /etc/iptables/rules.v4");
        } else {
            s.gap(format!("{} live iptables rule(s) are not in /etc/iptables/rules.v4: lost on reboot", missing.len()));
            s.code(&missing, 30);
        }
    } else if !custom.is_empty() && !managers.iter().any(|m| ["ufw", "nftables", "firewalld"].contains(&m.as_str())) {
        let appliers: Vec<&str> = enabled_custom
            .iter()
            .filter(|(_, text)| {
                text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                    .any(|w| ["iptables", "ip6tables", "nft", "iptables-restore", "ip6tables-restore"].contains(&w))
            })
            .map(|(unit, _)| unit.as_str())
            .collect();
        if appliers.is_empty() {
            s.gap(format!(
                "{} custom iptables rules are live but nothing saves or re-applies them: gone after a reboot",
                custom.len()
            ));
            s.code(&custom.iter().map(|r| (*r).clone()).collect::<Vec<_>>(), 30);
        } else {
            s.ok(format!(
                "{} custom iptables rules are re-applied at boot by {} (its rule files are restored with it)",
                custom.len(),
                appliers.join(", ")
            ));
        }
    }

    // docker itself
    if !view.text("docker/version").is_empty() {
        if enabled.contains("docker.service") || enabled.contains("docker.socket") {
            s.ok("docker.service is enabled");
        } else {
            s.gap("docker.service is not enabled: no container starts after a reboot");
        }
        for unit in custom_units.iter().filter(|u| u.contains("/docker.service.d/")) {
            let purpose: Vec<String> = view
                .file(unit)
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('[') && !l.starts_with('#'))
                .map(str::to_string)
                .collect();
            s.info(format!("docker.service drop-in `{}`: {}", unit_name(unit), purpose.join("; ")));
        }
    }
    if view.text("docker/swarm-state").trim() == "active" {
        let nodes = view.lines("docker/swarm-nodes.txt");
        if nodes.len() == 1 {
            s.ok("single-node swarm: the manager comes back by itself and restarts its services");
        } else {
            s.review(format!("swarm with {} nodes: services return only when a manager quorum is back", nodes.len()));
        }
        let services: Vec<String> =
            view.services().iter().filter_map(|x| x["name"].as_str().map(str::to_string)).collect();
        if services.iter().any(|n| n == "dokploy") && services.iter().any(|n| n == "dokploy-postgres") {
            s.info("Dokploy redeploys its services only once `dokploy-postgres` is up: both are swarm services and start together");
        }
    }
    s.or_nothing("nothing found that would be lost on a reboot")
}
