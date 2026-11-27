//! Firewall rules and who manages them. The stable copy drops what changes on its own: packet counters,
//! Docker's generated chains (never `DOCKER-USER`, which is the owner's), bridge-specific rules, and
//! fail2ban's ban entries. The raw rules go to `volatile/`.

use std::path::Path;

use super::collect::lines;
use super::{Collector, Ctx, Output, run_tool};
use crate::host::Host;

pub(crate) struct Firewall;

/// `iptables-save` output without counters, comments, Docker-generated chains and ban entries.
pub fn stable_rules(raw: &str) -> String {
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.starts_with('#') {
            continue;
        }
        // ":CHAIN POLICY [packets:bytes]" → drop the counters
        let line = match line.rfind(" [") {
            Some(i)
                if line.ends_with(']') && line[i + 2..line.len() - 1].split(':').all(|n| n.parse::<u64>().is_ok()) =>
            {
                &line[..i]
            }
            _ => line,
        };
        let docker_generated = line.contains("DOCKER") && !line.contains("DOCKER-USER");
        let bridge = line.contains("docker0") || has_bridge_id(line);
        let ban = line.starts_with("-A f2b-") && line.split_whitespace().nth(2) == Some("-s");
        if !(docker_generated || bridge || ban) {
            out.push(line);
        }
    }
    lines(out)
}

/// `br-<hex id>`: the interface of a user-defined Docker bridge.
fn has_bridge_id(line: &str) -> bool {
    line.match_indices("br-").any(|(i, _)| {
        let id: String = line[i + 3..].chars().take_while(char::is_ascii_hexdigit).collect();
        id.len() >= 12
    })
}

fn unit_is(host: &dyn Host, verb: &str, unit: &str) -> bool {
    run_tool(host, "systemctl", &[verb, "--quiet", unit]).is_some()
}

impl Collector for Firewall {
    fn name(&self) -> &'static str {
        "firewall"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let mut out = Output::default();
        for (tool, raw, stable) in [
            ("iptables-save", "iptables.raw", "firewall/iptables.rules"),
            ("ip6tables-save", "ip6tables.raw", "firewall/ip6tables.rules"),
        ] {
            if let Some(rules) = run_tool(host, tool, &[]) {
                out.stable(stable, stable_rules(&rules));
                out.volatile(raw, rules);
            }
        }
        if let Some(ruleset) = run_tool(host, "nft", &["list", "ruleset"]) {
            out.volatile("nftables.ruleset", ruleset);
        }
        if let Some(names) = run_tool(host, "ipset", &["list", "-n"]) {
            out.stable("firewall/ipset-names.txt", names);
        }
        let ufw = run_tool(host, "ufw", &["status", "verbose"]);
        if let Some(status) = &ufw {
            out.stable("firewall/ufw-status.txt", status.clone());
            out.stable("firewall/ufw-added.txt", run_tool(host, "ufw", &["show", "added"]).unwrap_or_default());
        }

        let mut managers = Vec::new();
        if ufw.is_some_and(|s| s.contains("Status: active")) {
            managers.push("ufw");
        }
        if unit_is(host, "is-active", "firewalld") {
            managers.push("firewalld");
        }
        if unit_is(host, "is-enabled", "nftables") {
            managers.push("nftables");
        }
        if unit_is(host, "is-enabled", "netfilter-persistent") || host.exists(Path::new("/etc/iptables/rules.v4")) {
            managers.push("iptables-persistent");
        }
        for unit in ["fail2ban", "crowdsec", "crowdsec-firewall-bouncer"] {
            if unit_is(host, "is-active", unit) {
                managers.push(unit);
            }
        }
        out.stable("firewall/managers.txt", lines(managers));

        if let Some(status) = run_tool(host, "fail2ban-client", &["status"]) {
            let jails =
                status.lines().find_map(|l| l.split_once("Jail list:")).map(|(_, list)| list).unwrap_or_default();
            out.stable("firewall/fail2ban-jails.txt", lines(jails.split(',').map(str::trim).filter(|j| !j.is_empty())));
            out.volatile("fail2ban-status.txt", status);
        }
        if let Some(collections) = run_tool(host, "cscli", &["collections", "list", "-o", "raw"]) {
            let rows = collections.lines().map(|l| l.split(',').take(2).collect::<Vec<_>>().join(","));
            out.stable("firewall/crowdsec-collections.txt", lines(rows));
            out.volatile("crowdsec-bouncers.txt", run_tool(host, "cscli", &["bouncers", "list"]).unwrap_or_default());
        }
        Ok(out)
    }
}
