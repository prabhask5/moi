//! Everything in the context file that is not Docker, Incus or monitoring.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{Doc, clip, shown};
use crate::audit::human;
use crate::backup::{Coverage, HOST_SYSTEM_PATHS, Identity, under};
use crate::config::AutoOr;
use crate::state::notes;

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

pub(super) fn owner_notes(doc: &mut Doc<'_>) {
    doc.heading(2, "Owner notes");
    let about_a_thing = |heading: &str| {
        ["Stack: ", "Container: ", "Service: ", "Unit: ", "Script: ", "Instance: "]
            .iter()
            .any(|p| heading.starts_with(p))
    };
    let general: Vec<(String, String)> =
        doc.notes.iter().filter(|(heading, _)| !about_a_thing(heading)).cloned().collect();
    for heading in notes::GENERAL {
        doc.used_notes.insert(heading.to_string());
    }
    if general.is_empty() {
        doc.line("_No notes yet: run `sudo moi notes` to say what this server is for._");
    }
    for (heading, body) in general {
        doc.used_notes.insert(heading.clone());
        doc.heading(3, &heading);
        doc.line(body);
    }
}

pub(super) fn system(doc: &mut Doc<'_>) {
    let v = doc.view;
    doc.heading(2, "System");
    let os: BTreeMap<String, String> = v
        .text("meta/os-release")
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, val)| (k.to_string(), val.trim_matches('"').to_string()))
        .collect();
    let field = |file: &str, key: &str| -> String {
        v.text(file)
            .lines()
            .find_map(|l| l.strip_prefix(key).map(|rest| rest.trim_start_matches(':').trim().to_string()))
            .unwrap_or_default()
    };
    let kb = |key: &str| {
        field("meta/memory", key)
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .map(|n| human(n * 1024))
            .unwrap_or_default()
    };
    let hardware: Vec<String> = ["sys_vendor", "product_name", "product_version"]
        .iter()
        .map(|k| field("meta/dmi", k))
        .filter(|x| !x.is_empty() && x != "None")
        .collect();
    let uptime = v
        .volatile("uptime")
        .split('.')
        .next()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|s| format!("{} days {} h", s / 86_400, s % 86_400 / 3600));
    let docker_version = v.text("docker/version").trim().to_string();
    let rows: Vec<Vec<String>> = [
        ("Hostname", v.hostname()),
        ("OS", os.get("PRETTY_NAME").cloned().unwrap_or_default()),
        ("Kernel", v.text("meta/kernel").trim().to_string()),
        ("Hardware", hardware.join(" ")),
        (
            "Virtualization",
            match v.text("meta/virt").trim() {
                "" => "none (bare metal)".to_string(),
                other => other.to_string(),
            },
        ),
        ("CPU", format!("{} — {} threads", field("meta/cpu", "model"), field("meta/cpu", "threads"))),
        ("RAM", kb("MemTotal")),
        ("Swap", kb("SwapTotal")),
        ("Timezone", v.text("meta/timezone").trim().to_string()),
        ("Uptime", uptime.unwrap_or_default()),
        (
            "Docker",
            if docker_version.is_empty() {
                "not installed".to_string()
            } else {
                format!(
                    "{docker_version} (compose {}), data root {}",
                    v.text("docker/compose-version").trim(),
                    v.text("docker/root-dir").trim()
                )
            },
        ),
    ]
    .into_iter()
    .filter(|(_, value)| !value.trim().is_empty() && value.trim() != "—  threads")
    .map(|(k, value)| vec![k.to_string(), value])
    .collect();
    doc.table(&["", ""], &rows);
}

/// Rows of the disk table from `lsblk -J`.
fn lsblk_rows(devices: &Value, parent_kind: &str, out: &mut Vec<Vec<String>>) {
    for d in devices.as_array().into_iter().flatten() {
        let kind = match (text(&d["tran"]), d["rota"].as_bool()) {
            ("nvme", _) => "NVMe".to_string(),
            ("usb", Some(true)) => "HDD (USB)".to_string(),
            ("usb", _) => "USB".to_string(),
            (_, Some(true)) => "HDD".to_string(),
            (_, Some(false)) => "SSD".to_string(),
            _ => parent_kind.to_string(),
        };
        out.push(vec![
            text(&d["path"]).to_string(),
            d["size"].as_u64().map(human).unwrap_or_default(),
            text(&d["type"]).to_string(),
            kind.clone(),
            text(&d["fstype"]).to_string(),
            text(&d["label"]).to_string(),
            text(&d["mountpoint"]).to_string(),
            text(&d["model"]).to_string(),
            text(&d["uuid"]).to_string(),
        ]);
        lsblk_rows(&d["children"], &kind, out);
    }
}

pub(super) fn storage(doc: &mut Doc<'_>) {
    let v = doc.view;
    doc.heading(2, "Storage");
    let mut rows = Vec::new();
    lsblk_rows(&v.json("storage/lsblk.json")["blockdevices"], "", &mut rows);
    if !rows.is_empty() {
        doc.heading(3, "Disks & partitions");
        doc.table(&["Device", "Size", "Type", "Kind", "FS", "Label", "Mounted at", "Model", "UUID"], &rows);
    }
    let fstab: Vec<String> = v
        .file("/etc/fstab")
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(str::to_string)
        .collect();
    if !fstab.is_empty() {
        doc.heading(3, "/etc/fstab");
        doc.code(&fstab.join("\n"), 60);
    }
    let df = v.volatile("df.txt");
    if !df.trim().is_empty() {
        doc.heading(3, "Usage");
        doc.code(&df, 60);
        for line in df.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if let (Some(used), Some(mount)) = (f.get(5), f.get(6))
                && used.trim_end_matches('%').parse::<u32>().is_ok_and(|p| p >= 85)
            {
                doc.warn(format!("{mount} is {used} full"));
            }
        }
    }
    for (file, title) in [
        ("mdadm-scan.txt", "mdadm RAID"),
        ("lvm.txt", "LVM"),
        ("zpool-list.txt", "ZFS pools"),
        ("zfs-datasets.txt", "ZFS datasets"),
        ("btrfs-subvolumes.txt", "btrfs subvolumes"),
    ] {
        let t = v.text(&format!("storage/{file}"));
        if !t.trim().is_empty() {
            doc.heading(3, title);
            doc.code(&t, 60);
        }
    }
    let degraded = |t: &str| {
        t.contains("DEGRADED")
            || t.contains("FAULTED")
            || t.split('[').skip(1).any(|part| {
                part.split(']')
                    .next()
                    .is_some_and(|inner| inner.contains('_') && inner.chars().all(|c| c == 'U' || c == '_'))
            })
    };
    for file in ["mdstat.txt", "zpool-status.txt"] {
        if degraded(&v.volatile(file)) {
            doc.warn(format!("a storage array reports a problem (see `{file}`)"));
        }
    }
    if v.volatile("smart.txt").contains("FAILED") {
        doc.warn("a disk reports SMART FAILED");
    }

    let trees = v.host().read_dir(&v.state().volatile().join("trees")).unwrap_or_default();
    if !trees.is_empty() {
        doc.heading(3, "Directory structure (data roots, top 2 levels)");
        for file in trees {
            let rows: Vec<Vec<String>> = v
                .volatile_lines(&format!("trees/{file}"))
                .iter()
                .map(|l| l.split('\t').map(str::to_string).collect())
                .collect();
            let Some(root) = rows.first().and_then(|r| r.get(3)).cloned() else { continue };
            let mut entries = Vec::new();
            for row in rows.iter().skip(1) {
                let [mode, user, group, path] = row.as_slice() else { continue };
                let Some(rel) = path.strip_prefix(&format!("{}/", root.trim_end_matches('/'))) else { continue };
                let depth = rel.matches('/').count() + 1;
                if depth > 2 {
                    continue;
                }
                let name = rel.rsplit('/').next().unwrap_or(rel);
                let owner = if depth == 1 && (user != "root" || group != "root") {
                    format!(" ({user}:{group} {mode})")
                } else {
                    String::new()
                };
                entries.push(format!("{}- {name}/{owner}", "  ".repeat(depth - 1)));
            }
            doc.line(format!("`{root}` — {} directories recorded", rows.len()));
            for entry in entries.iter().take(120) {
                doc.line(entry);
            }
            if entries.len() > 120 {
                doc.line(format!("… {} more entries", entries.len() - 120));
            }
            doc.blank();
        }
    }
    let fungible = &doc.config.data;
    if !fungible.fungible.is_empty() {
        doc.heading(3, "Fungible data (never backed up, by decision)");
        let lines: Vec<String> = fungible
            .fungible
            .iter()
            .map(|p| match fungible.fungible_reason.get(p) {
                Some(reason) => format!("- `{p}` — {reason}"),
                None => format!("- `{p}`"),
            })
            .collect();
        for l in lines {
            doc.line(l);
        }
    }
}

pub(super) fn network(doc: &mut Doc<'_>) {
    let v = doc.view;
    doc.heading(2, "Network");
    for (file, title) in [("addresses.txt", "Interfaces"), ("routes.txt", "Routes"), ("dns.txt", "DNS")] {
        let t = v.text(&format!("network/{file}"));
        if !t.trim().is_empty() {
            doc.line(format!("**{title}**"));
            doc.code(&t, 40);
        }
    }
    let tailscale = v.text("network/tailscale-ip.txt");
    if !tailscale.trim().is_empty() {
        doc.line(format!("**Tailscale IP:** {}", tailscale.trim()));
    }
    let wireguard = v.text("network/wireguard-interfaces.txt");
    if !wireguard.trim().is_empty() {
        doc.line(format!("**WireGuard interfaces:** {} (configuration in /etc/wireguard)", wireguard.trim()));
    }
    let mut rows: Vec<(u32, Vec<String>)> = Vec::new();
    for line in v.volatile_lines("listening.txt") {
        let mut f = line.splitn(3, ' ');
        let (Some(proto), Some(address)) = (f.next(), f.next()) else { continue };
        let processes: std::collections::BTreeSet<&str> =
            f.next().unwrap_or_default().split('"').skip(1).step_by(2).collect();
        let (addr, port) = address.rsplit_once(':').unwrap_or((address, ""));
        rows.push((
            port.parse().unwrap_or(99_999),
            vec![
                proto.to_string(),
                addr.to_string(),
                port.to_string(),
                processes.into_iter().collect::<Vec<_>>().join(", "),
            ],
        ));
    }
    if !rows.is_empty() {
        rows.sort();
        doc.heading(3, "Listening ports (host)");
        doc.table(&["Proto", "Address", "Port", "Process"], &rows.into_iter().map(|(_, r)| r).collect::<Vec<_>>());
    }
}

/// Host names inside a Traefik rule: every back-quoted or quoted string within `Host(…)`.
fn traefik_hosts(rule: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = rule;
    while let Some(i) = rest.find("Host") {
        let after = &rest[i + 4..];
        let after = after.strip_prefix("Regexp").unwrap_or(after);
        rest = after;
        let Some(args) = after.strip_prefix('(').and_then(|a| a.split(')').next()) else { continue };
        let mut parts = args.split(['`', '"', '\'']);
        parts.next();
        while let Some(host) = parts.next() {
            if !host.is_empty() {
                out.push(host.to_string());
            }
            parts.next();
        }
    }
    out
}

pub(super) fn routes(doc: &mut Doc<'_>) {
    let v = doc.view;
    let mut routes: std::collections::BTreeSet<(String, String, String)> = std::collections::BTreeSet::new();
    let labelled: Vec<(String, Value)> = v
        .containers()
        .into_iter()
        .map(|c| (text(&c["name"]).to_string(), c["labels"].clone()))
        .chain(v.services().into_iter().flat_map(|s| {
            let name = format!("service {}", text(&s["name"]));
            [(name.clone(), s["labels"].clone()), (name, s["container_labels"].clone())]
        }))
        .collect();
    for (name, labels) in labelled {
        for (key, value) in labels.as_object().into_iter().flatten() {
            if let Some(router) = key.strip_prefix("traefik.http.routers.").and_then(|k| k.strip_suffix(".rule")) {
                for host in traefik_hosts(text(value)) {
                    routes.insert((host, name.clone(), format!("traefik label (router {router})")));
                }
            }
            if key == "caddy" || key.strip_prefix("caddy_").is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())) {
                for host in text(value).replace(',', " ").split_whitespace() {
                    routes.insert((host.to_string(), name.clone(), "caddy-docker-proxy label".to_string()));
                }
            }
        }
    }
    // files: Traefik dynamic configuration, cloudflared ingress, nginx server_name, Caddyfile sites
    for file in v.lines("files.manifest.tsv").iter().filter_map(|l| l.split('\t').nth(3).map(str::to_string)) {
        let name = file.rsplit('/').next().unwrap_or_default();
        let yaml = name.ends_with(".yml") || name.ends_with(".yaml");
        let relevant = (yaml && (file.contains("traefik") || file.contains("cloudflared")))
            || name.starts_with("Caddyfile")
            || (file.contains("nginx") && (name.ends_with(".conf") || file.contains("sites-enabled")));
        if !relevant {
            continue;
        }
        let Some(content) = v.file(&file) else { continue };
        let mut tunnel_host: Option<String> = None;
        let mut depth = 0i32;
        for line in content.lines() {
            let l = line.split('#').next().unwrap_or_default().trim();
            if file.contains("traefik") && l.contains("Host") {
                for host in traefik_hosts(l) {
                    routes.insert((host, String::new(), format!("traefik file {file}")));
                }
            }
            if file.contains("cloudflared") {
                let item = l.trim_start_matches("- ").trim();
                if let Some(h) = item.strip_prefix("hostname:") {
                    tunnel_host = Some(h.trim().trim_matches(['"', '\'']).to_string());
                } else if let (Some(service), Some(h)) = (item.strip_prefix("service:"), tunnel_host.take()) {
                    routes.insert((h, service.trim().to_string(), format!("cloudflared tunnel {file}")));
                }
            }
            if file.contains("nginx")
                && let Some(names) = l.strip_prefix("server_name")
            {
                for host in names.trim_end_matches(';').split_whitespace().filter(|h| *h != "_") {
                    routes.insert((host.to_string(), String::new(), format!("nginx {file}")));
                }
            }
            if name.starts_with("Caddyfile") {
                if depth == 0 && l.ends_with('{') && !l.starts_with(['(', '{']) {
                    for host in l.trim_end_matches('{').replace(',', " ").split_whitespace() {
                        routes.insert((host.to_string(), String::new(), format!("Caddyfile {file}")));
                    }
                }
                depth += l.matches('{').count() as i32 - l.matches('}').count() as i32;
            }
        }
    }
    if !routes.is_empty() {
        doc.heading(2, "Domains / reverse-proxy routes");
        doc.table(
            &["Host", "Target", "Defined in"],
            &routes.into_iter().map(|(a, b, c)| vec![a, b, c]).collect::<Vec<_>>(),
        );
    }
}

pub(super) fn moi(doc: &mut Doc<'_>) {
    let c = doc.config;
    doc.heading(2, "moi");
    doc.line(format!(
        "Version {} · configuration `/etc/moi/moi.toml` (secrets in `secrets.toml`, never printed here) · state `{}`.",
        env!("CARGO_PKG_VERSION"),
        c.state.dir
    ));
    let mut rows = Vec::new();
    let mut check = |name: &str, enabled: bool, interval: String, monitor: &Option<String>, tag: &str| {
        if enabled {
            rows.push(vec![
                name.to_string(),
                interval,
                monitor.clone().unwrap_or_else(|| "— (only in `moi check`)".to_string()),
                tag.to_string(),
            ]);
        }
    };
    let ck = &c.checks;
    check(
        "resources",
        ck.resources.common.enabled,
        ck.resources.common.interval.to_string(),
        &ck.resources.common.monitor,
        &ck.resources.common.journal_tag,
    );
    check(
        "storage",
        ck.storage.common.enabled,
        ck.storage.common.interval.to_string(),
        &ck.storage.common.monitor,
        &ck.storage.common.journal_tag,
    );
    check(
        "services",
        ck.services.common.enabled,
        ck.services.common.interval.to_string(),
        &ck.services.common.monitor,
        &ck.services.common.journal_tag,
    );
    if ck.edge.enabled {
        check(
            "edge",
            true,
            "with services".to_string(),
            &ck.edge.monitor.clone().or_else(|| ck.services.common.monitor.clone()),
            "",
        );
    }
    for custom in &ck.custom {
        check(
            &custom.name,
            custom.common.enabled,
            custom.common.interval.to_string(),
            &custom.common.monitor,
            &custom.common.journal_tag,
        );
    }
    if rows.is_empty() {
        doc.line("- Health checks: none configured.");
    } else {
        doc.heading(3, "Health checks (which check feeds which monitor)");
        doc.table(&["Check", "Every", "Uptime Kuma monitor", "Journal tag"], &rows);
        if ck.services.common.enabled {
            doc.line(format!(
                "`services` runs {} HTTP and {} exec probes; change them in `[checks.services]`, not in scripts.",
                ck.services.http.len(),
                ck.services.exec.len()
            ));
        }
    }
    let b = &c.backup;
    if b.enabled {
        let offsite = if b.offsite.enabled {
            format!(
                "off-site with restic (tag `{}`, at most one upload per {}, prune on {})",
                b.offsite.tag, b.offsite.min_interval, b.offsite.prune_on
            )
        } else {
            "no off-site copy".to_string()
        };
        doc.line(format!(
            "- **Backup**: daily at `{}` to `{}` (kept {}, at most {}), {offsite}. Monitor: {}.",
            b.schedule,
            b.local_dir.clone().unwrap_or_default(),
            b.keep_local,
            b.max_local,
            ck.backup.monitor.clone().unwrap_or_else(|| "none".to_string())
        ));
    } else {
        doc.line("- **Backup**: not configured (`[backup]`).");
    }
    if !c.heal.is_empty() {
        doc.line(format!(
            "- **Self-healing rules**: {}.",
            c.heal.iter().map(|h| format!("`{}`", h.name)).collect::<Vec<_>>().join(", ")
        ));
    }
    if !c.boot.compose_up.is_empty() || !c.boot.wait_for_mounts.is_empty() {
        doc.line(format!(
            "- **Boot**: waits for {}, then starts the stacks in {}; verifies after {}.",
            if c.boot.wait_for_mounts.is_empty() {
                "nothing".to_string()
            } else {
                c.boot.wait_for_mounts.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ")
            },
            c.boot.compose_up.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", "),
            c.boot.verify_after
        ));
    }
    if c.firewall.manage {
        doc.line(format!("- **Firewall**: moi owns the chains {} (rules in `{}` and `{}`) and applies them at boot; Docker owns the rest.", c.firewall.chains.join(", "), c.firewall.rules_v4, c.firewall.rules_v6));
    }
    if c.monitors.kuma.enabled {
        doc.line(format!("- **Monitors**: Uptime Kuma at {} ({} mode). moi adopts existing monitors by name and push token and never recreates them.", c.monitors.kuma.url.clone().unwrap_or_default(), if c.monitors.mode == crate::config::MonitorMode::Live { "live" } else { "shadow" }));
    }
}

pub(super) fn firewall(doc: &mut Doc<'_>) {
    let v = doc.view;
    doc.heading(2, "Firewall & access control");
    let managers = v.lines("firewall/managers.txt");
    let managed_by_moi = if doc.config.firewall.manage { " — the owned chains are applied by moi" } else { "" };
    doc.line(format!(
        "Active managers: {}{managed_by_moi}",
        if managers.is_empty() {
            "none detected (raw iptables / Docker only)".to_string()
        } else {
            managers.join(", ")
        }
    ));
    let ufw = v.text("firewall/ufw-status.txt");
    if !ufw.trim().is_empty() {
        doc.heading(3, "ufw");
        doc.code(&ufw, 120);
    }
    let rules = v.text("firewall/iptables.rules");
    let mut table = "";
    let mut filter = Vec::new();
    for line in rules.lines() {
        if let Some(name) = line.strip_prefix('*') {
            table = name;
        }
        let chain_with_policy = line.starts_with(':') && !line.ends_with("ACCEPT");
        if table == "filter"
            && (line.starts_with("-A") || chain_with_policy)
            && !["-A ufw", "-A f2b-"].iter().any(|p| line.starts_with(p))
        {
            filter.push(line);
        }
    }
    if !filter.is_empty() {
        doc.heading(3, "iptables (filter table, without Docker's generated chains and ufw/fail2ban internals)");
        doc.code(&filter.join("\n"), 150);
        if filter.iter().any(|l| l.starts_with("-A DOCKER-USER") && !l.ends_with("-j RETURN")) {
            doc.line("_Custom DOCKER-USER rules exist: they filter traffic to published container ports._");
        }
    }
    if managers.iter().any(|m| m == "nftables")
        && let Some(conf) = v.file("/etc/nftables.conf")
    {
        doc.heading(3, "/etc/nftables.conf");
        doc.code(&conf, 150);
    }
    for (file, label) in [("firewall/fail2ban-jails.txt", "fail2ban jails"), ("firewall/ipset-names.txt", "ipsets")] {
        let items = v.lines(file);
        if !items.is_empty() {
            doc.line(format!("**{label}:** {}", items.join(", ")));
        }
    }
    let crowdsec: Vec<String> = v
        .lines("firewall/crowdsec-collections.txt")
        .iter()
        .filter(|l| !l.starts_with("name"))
        .filter_map(|l| l.split(',').next().map(str::to_string))
        .collect();
    if !crowdsec.is_empty() {
        doc.line(format!("**CrowdSec collections:** {}", crowdsec.join(", ")));
    }
    // sshd: the first occurrence of a keyword wins, drop-ins are read before the main file
    let mut sshd: BTreeMap<String, String> = BTreeMap::new();
    let mut files = v.captured_in("/etc/ssh/sshd_config.d");
    files.push("/etc/ssh/sshd_config".to_string());
    for file in files {
        for line in v.file(&file).unwrap_or_default().lines() {
            let mut words = line.split_whitespace();
            let (Some(key), Some(value)) = (words.next(), words.next()) else { continue };
            let lower = key.to_lowercase();
            if [
                "port",
                "permitrootlogin",
                "passwordauthentication",
                "pubkeyauthentication",
                "allowusers",
                "allowgroups",
                "listenaddress",
                "kbdinteractiveauthentication",
            ]
            .contains(&lower.as_str())
            {
                sshd.entry(lower).or_insert_with(|| value.to_string());
            }
        }
    }
    if !sshd.is_empty() {
        doc.line(format!(
            "**sshd:** {}",
            sshd.iter().map(|(k, val)| format!("{k}={val}")).collect::<Vec<_>>().join(", ")
        ));
        if sshd.get("passwordauthentication").is_none_or(|val| val.eq_ignore_ascii_case("yes")) {
            doc.warn("SSH password authentication is enabled (or not explicitly disabled)");
        }
    }
    doc.line("_Not captured here: router/NAT port forwards, Cloudflare (DNS, WAF, Access), provider firewalls._");
}

pub(super) fn jobs(doc: &mut Doc<'_>) {
    let v = doc.view;
    doc.heading(2, "Scheduled jobs");
    let jobs = v.cron_jobs();
    if !jobs.is_empty() {
        doc.heading(3, "cron");
        let rows: Vec<Vec<String>> = jobs
            .iter()
            .map(|j| vec![j.source.clone(), j.schedule.clone(), j.user.clone(), clip(&shown(&j.command), 200)])
            .collect();
        doc.table(&["Source", "Schedule", "User", "Command"], &rows);
    }
    let mut run_parts = Vec::new();
    for period in ["hourly", "daily", "weekly", "monthly"] {
        let files: Vec<String> = v
            .captured_in(&format!("/etc/cron.{period}"))
            .iter()
            .filter_map(|p| p.rsplit('/').next().map(str::to_string))
            .filter(|n| n != ".placeholder")
            .collect();
        if !files.is_empty() {
            run_parts.push(format!("- cron.{period}: {}", files.join(", ")));
        }
    }
    if !run_parts.is_empty() {
        doc.line("**run-parts directories:**");
        for l in run_parts {
            doc.line(l);
        }
    }
    let timers = v.lines("cron/enabled-timers.txt");
    if !timers.is_empty() {
        let mut rows = Vec::new();
        for timer in &timers {
            let info = v.unit_info(&format!("/etc/systemd/system/{timer}"));
            let service = v.unit_info(&format!("/etc/systemd/system/{}", timer.replace(".timer", ".service")));
            let schedule: Vec<String> = ["OnCalendar", "OnBootSec", "OnUnitActiveSec"]
                .iter()
                .flat_map(|k| {
                    info.get(*k).cloned().unwrap_or_default().into_iter().map(move |val| format!("{k}={val}"))
                })
                .collect();
            rows.push(vec![
                timer.clone(),
                if info.is_empty() { "package" } else { "custom" }.to_string(),
                schedule.join(" "),
                clip(&shown(&service.get("ExecStart").cloned().unwrap_or_default().join(" ; ")), 160),
            ]);
        }
        doc.heading(3, "systemd timers (enabled)");
        doc.table(&["Timer", "Origin", "Schedule", "Runs"], &rows);
    }
    if jobs.is_empty() && timers.is_empty() {
        doc.line("_None._");
    }
}

pub(super) fn units(doc: &mut Doc<'_>) {
    let v = doc.view;
    doc.heading(2, "Services (systemd)");
    let custom = v.custom_units();
    let enabled: std::collections::BTreeSet<String> = v.lines("systemd/enabled-units.txt").into_iter().collect();
    let running: std::collections::BTreeSet<String> = v
        .volatile_lines("running-services.txt")
        .iter()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect();
    let base = |path: &str| path.rsplit('/').next().unwrap_or(path).to_string();
    if !custom.is_empty() {
        doc.heading(3, "Custom units (/etc/systemd/system)");
        for unit in &custom {
            if unit.ends_with(".conf") {
                doc.line(format!("- drop-in `{unit}`"));
                continue;
            }
            let name = base(unit);
            let info = v.unit_info(unit);
            let state = format!(
                "{}{}",
                if enabled.contains(&name) { "enabled" } else { "not enabled" },
                if running.contains(&name) { ", running" } else { "" }
            );
            doc.line(format!(
                "- **{name}** ({state}) — {}",
                info.get("Description").map(|d| d.join("; ")).unwrap_or_default()
            ));
            for key in
                ["ExecStart", "User", "WorkingDirectory", "OnCalendar", "OnBootSec", "PathChanged", "What", "Where"]
            {
                if let Some(values) = info.get(key) {
                    doc.line(format!("  - {key}: `{}`", clip(&shown(&values.join(" ; ")), 200)));
                }
            }
            doc.write_note(&format!("Unit: {name}"));
        }
    }
    let other: Vec<&String> =
        enabled.iter().filter(|u| u.ends_with(".service") && !custom.iter().any(|c| base(c) == **u)).collect();
    if !other.is_empty() {
        doc.blank();
        doc.line(format!(
            "**Other enabled services:** {}",
            other.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    let failed: Vec<String> = v
        .volatile_lines("failed-units.txt")
        .iter()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect();
    if !failed.is_empty() {
        doc.line(format!("**Failed units:** {}", failed.join(", ")));
        for unit in failed {
            doc.warn(format!("systemd unit failed: {unit}"));
        }
    }
}

/// The first comment lines of a script, as its description.
fn script_description(content: &str) -> String {
    let mut out = Vec::new();
    for line in content.lines().skip(1).take(14) {
        let s = line.trim();
        if let Some(comment) = s.strip_prefix('#') {
            let comment = comment.trim_start_matches('#').trim();
            if !comment.is_empty() && !["shellcheck", "-*-", "vim:"].iter().any(|p| comment.starts_with(p)) {
                out.push(comment.to_string());
            }
        } else if !s.is_empty() {
            break;
        }
        if out.len() >= 3 {
            break;
        }
    }
    clip(&out.join(" "), 200)
}

pub(super) fn scripts(doc: &mut Doc<'_>) {
    let v = doc.view;
    let index = v.lines("scripts/index.txt");
    if index.is_empty() {
        return;
    }
    doc.heading(2, "Scripts");
    let mut rows = Vec::new();
    for path in index.iter().take(150) {
        let content = v.file(path).unwrap_or_default();
        let has_word = |words: &[&str]| {
            content.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).any(|w| words.contains(&w))
        };
        let mut tags = Vec::new();
        if has_word(&["rclone", "restic", "borg", "kopia", "duplicity", "rsync"]) {
            tags.push("backup/sync");
        }
        if has_word(&["docker"]) {
            tags.push("docker");
        }
        let description =
            doc.note_text(&format!("Script: {path}")).map_or_else(|| script_description(&content), |n| clip(&n, 300));
        rows.push(vec![
            path.clone(),
            shown(&description),
            v.callers(path).join(", "),
            tags.join(", "),
            content.lines().count().to_string(),
        ]);
    }
    doc.table(&["Script", "Description", "Called by", "Tags", "Lines"], &rows);
    if index.len() > 150 {
        doc.line(format!(
            "… {} more in `{}`",
            index.len() - 150,
            v.state().snapshot().join("scripts/index.txt").display()
        ));
    }
}

pub(super) fn software(doc: &mut Doc<'_>) {
    let v = doc.view;
    doc.heading(2, "Installed software");
    let manual = v.lines("packages/apt-manual.txt");
    if !manual.is_empty() {
        doc.line(format!("**apt (manually installed, {}):** {}", manual.len(), manual.join(", ")));
    }
    let held = v.lines("packages/apt-hold.txt");
    if !held.is_empty() {
        doc.line(format!("**apt (held back):** {}", held.join(", ")));
    }
    for (file, title) in [
        ("snap.txt", "snaps"),
        ("flatpak.txt", "flatpaks"),
        ("pipx.txt", "pipx"),
        ("npm-global.txt", "npm -g"),
        ("cargo.txt", "cargo install"),
    ] {
        let items = v.lines(&format!("packages/{file}"));
        if !items.is_empty() {
            doc.line(format!("**{title}:** {}", items.join(", ")));
        }
    }
    let binaries: Vec<String> = v
        .lines("packages/usr-local-bin.tsv")
        .iter()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(_, kind)| kind.starts_with("ELF") || *kind == "data")
        .filter_map(|(path, _)| path.rsplit('/').next().map(str::to_string))
        .collect();
    if !binaries.is_empty() {
        doc.line(format!("**Binaries installed by hand in /usr/local/(s)bin:** {}", binaries.join(", ")));
    }
    let opt = v.lines("packages/opt-listing.txt");
    if !opt.is_empty() {
        doc.line(format!("**/opt:** {}", opt.join(", ")));
    }
}

pub(super) fn users(doc: &mut Doc<'_>) {
    let v = doc.view;
    doc.heading(2, "Users & access");
    let groups: Vec<Vec<String>> =
        v.lines("users/groups.group").iter().map(|l| l.split(':').map(str::to_string).collect()).collect();
    let mut rows = Vec::new();
    for line in v.lines("users/users.passwd") {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 {
            continue;
        }
        let member_of: Vec<&str> = groups
            .iter()
            .filter(|g| g.get(3).is_some_and(|members| members.split(',').any(|m| m == f[0])))
            .filter_map(|g| g.first().map(String::as_str))
            .collect();
        // key comments only: the keys themselves say nothing a reader needs
        let keys: Vec<String> = v
            .file(&format!("{}/.ssh/authorized_keys", f[5].trim_end_matches('/')))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .map(|l| {
                let words: Vec<&str> = l.split_whitespace().collect();
                if words.len() > 2 { words[2..].join(" ") } else { "(no comment)".to_string() }
            })
            .collect();
        rows.push(vec![
            f[0].to_string(),
            f[2].to_string(),
            f[5].to_string(),
            f[6].to_string(),
            member_of.join(", "),
            if keys.is_empty() { String::new() } else { format!("{}: {}", keys.len(), keys.join(", ")) },
        ]);
    }
    doc.table(&["User", "UID", "Home", "Shell", "Groups", "SSH keys"], &rows);
    ssh_access(doc);
}

/// How the server is reached from outside (16 § Records): from what the `ssh` collector recorded.
fn ssh_access(doc: &mut Doc<'_>) {
    let access = doc.view.json("ssh/access.json");
    let (Some(hostname), Some(alias)) = (access["hostname"].as_str(), access["alias"].as_str()) else { return };
    doc.heading(3, "SSH access");
    let user = access["user"].as_str().unwrap_or("root");
    let via = if access["provider"] == "none" {
        format!("`{hostname}` (reachable as it is; moi set up nothing in the path)")
    } else {
        format!(
            "`{hostname}`: a Cloudflare Tunnel whose connector runs on the host as `cloudflared.service` and hands connections to sshd on port 22 — independent of Docker and the reverse proxy"
        )
    };
    doc.line(format!(
        "- **This server:** `ssh {alias}` (user `{user}`) from a computer set up with `moi ssh connect`, via {via}."
    ));
    if let Some(rules) = access["ingress"].as_array().filter(|r| !r.is_empty()) {
        let list: Vec<String> = rules
            .iter()
            .map(|r| match r["hostname"].as_str() {
                Some(name) => format!("`{name}` → `{}`", r["service"].as_str().unwrap_or_default()),
                None => format!("everything else → `{}`", r["service"].as_str().unwrap_or_default()),
            })
            .collect();
        doc.line(format!(
            "- **Tunnel ingress** (in this order): {}. moi owns only the rule of `{hostname}`.",
            list.join("; ")
        ));
    }
    if let Some(dns) = access["dns"].as_str() {
        doc.line(format!("- **DNS:** {dns}."));
    }
    let sshd = &access["sshd"];
    if sshd.is_object() {
        doc.line(format!(
            "- **sshd:** passwords `{}`, root login `{}`.",
            sshd["passwordauthentication"].as_str().unwrap_or("?"),
            sshd["permitrootlogin"].as_str().unwrap_or("?")
        ));
    }
    let keys: Vec<String> = access["authorized_keys"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|k| {
            format!(
                "{} `{}`",
                k["comment"].as_str().filter(|c| !c.is_empty()).unwrap_or("(no comment)"),
                k["fingerprint"].as_str().unwrap_or_default()
            )
        })
        .collect();
    if !keys.is_empty() {
        doc.line(format!(
            "- **Keys allowed to log in as `{user}`:** {}. Add one with `moi ssh authorize`.",
            keys.join("; ")
        ));
    }
    // the way into a VM when its own path is down: through this server
    let instances: Vec<String> = doc
        .view
        .dir("incus/instances")
        .into_iter()
        .filter_map(|f| f.strip_suffix(".yaml").map(str::to_string))
        .collect();
    for instance in &instances {
        doc.line(format!("- **Rescue for the VM `{instance}`** (when it cannot be reached on its own path): `ssh {alias}`, then `sudo incus exec {instance} -- bash`."));
    }
    doc.line("- Changing this path through itself is safe only with `moi ssh up` / `moi ssh down` (they arm a rollback and want `moi ssh confirm` from a new connection). Do not stop `cloudflared.service` or edit the tunnel's rule for this name by hand over SSH.");
}

pub(super) fn backups(doc: &mut Doc<'_>) {
    let v = doc.view;
    let identity = Identity::compute(doc.config, &v);
    doc.heading(2, "Backups & rebuild");
    let b = &doc.config.backup;
    if b.enabled {
        doc.line(format!(
            "- **What is backed up** (computed fresh on every run; `moi backup --dry-run` shows it): database dumps of {}, the paths below, and moi's own state — never anything declared fungible.",
            match &b.databases {
                AutoOr::Auto => "every Postgres/MySQL/MariaDB container and swarm service".to_string(),
                AutoOr::List(names) => names.join(", "),
            }
        ));
        let dumped: Vec<&str> = identity.databases.iter().filter(|d| d.dumped).map(|d| d.name.as_str()).collect();
        if !dumped.is_empty() {
            doc.line(format!("- **Databases dumped:** {}", dumped.join(", ")));
        }
    } else {
        doc.line("- **No backup is configured** (`[backup]` in moi.toml): nothing on this server is protected by moi.");
    }
    // legacy or foreign backup jobs still on the machine
    let backup_words = ["backup", "restic", "rclone", "borg", "kopia", "duplicati"];
    let jobs: Vec<String> = v
        .cron_jobs()
        .iter()
        .filter(|j| backup_words.iter().any(|w| j.command.to_lowercase().contains(w)))
        .map(|j| format!("- `{}` ({}) — `{}`", j.schedule, j.source, clip(&shown(&j.command), 200)))
        .collect();
    if !jobs.is_empty() {
        doc.line("**Other backup-related jobs:**");
        for j in jobs {
            doc.line(j);
        }
    }

    // where data lives, and whether the backup has it
    let mut rows: std::collections::BTreeSet<Vec<String>> = std::collections::BTreeSet::new();
    let mut add = |path: &str, what: String, read_only: bool| {
        if path.is_empty() || HOST_SYSTEM_PATHS.iter().any(|p| under(path, p)) {
            return;
        }
        let mark = match identity.coverage(path) {
            Coverage::Covered(_) => "✓".to_string(),
            Coverage::Fungible(reason) if reason.is_empty() => "fungible".to_string(),
            Coverage::Fungible(reason) => format!("fungible ({reason})"),
            Coverage::Dumped(db) => format!("✓ dump of {db}"),
            Coverage::UndumpedDatabase(_) => "✗ database, not dumped".to_string(),
            Coverage::NotCovered if read_only => "— (read-only view)".to_string(),
            Coverage::NotCovered => "✗".to_string(),
        };
        rows.insert(vec![path.to_string(), what, mark]);
    };
    for c in v.containers() {
        for m in c["mounts"].as_array().into_iter().flatten() {
            match text(&m["type"]) {
                "bind" => add(
                    text(&m["source"]),
                    format!("bind mount → {}:{}", text(&c["name"]), text(&m["destination"])),
                    m["ro"] == true,
                ),
                "volume" if !text(&m["name"]).is_empty() => add(
                    &format!("/var/lib/docker/volumes/{}/_data", text(&m["name"])),
                    format!("volume {} → {}", text(&m["name"]), text(&c["name"])),
                    m["ro"] == true,
                ),
                _ => {}
            }
        }
    }
    for s in v.services() {
        for m in s["mounts"].as_array().into_iter().flatten() {
            match text(&m["type"]) {
                "bind" => add(
                    text(&m["source"]),
                    format!("bind mount → service {}:{}", text(&s["name"]), text(&m["destination"])),
                    m["ro"] == true,
                ),
                "volume" => add(
                    &format!("/var/lib/docker/volumes/{}/_data", text(&m["source"])),
                    format!("volume {} → service {}", text(&m["source"]), text(&s["name"])),
                    m["ro"] == true,
                ),
                _ => {}
            }
        }
    }
    for p in v.compose_projects() {
        add(text(&p["workdir"]), format!("compose stack dir ({})", text(&p["project"])), false);
    }
    for instance in v.incus_instances() {
        add("/var/lib/incus", format!("Incus instance {instance} (disk + config)"), false);
    }
    if !rows.is_empty() {
        doc.heading(3, "Where data lives");
        doc.table(&["Path", "What", "In backup"], &rows.into_iter().collect::<Vec<_>>());
    }
    doc.blank();
    doc.line("**Rebuild on new hardware:** install moi, `moi pull` (downloads the state bundle), then `moi restore` — phase by phase, databases first — and `moi verify` after the guided reboot. `moi restore --list` shows the phases.");
    doc.line(format!("**Not captured by moi:** container images (pulled again), anything declared fungible, DNS/Cloudflare settings, router configuration, BIOS settings, files over {} outside the backup paths.", doc.config.capture.max_file));
}

pub(super) fn audit(doc: &mut Doc<'_>) {
    let v = doc.view;
    let text = v.host().read_to_string(&v.state().audit()).unwrap_or_default();
    if text.trim().is_empty() {
        return;
    }
    // demote every heading by one level: the audit's title becomes a section of this file
    doc.blank();
    for line in text.lines() {
        if line.starts_with('#') {
            doc.blank();
            doc.line(format!("#{line}"));
        } else {
            doc.line(line);
        }
        if let Some(gap) = line.strip_prefix("- ✗ ") {
            doc.warn(format!("audit: {}", clip(&gap.replace("**", ""), 180)));
        }
    }
}

pub(super) fn changes(doc: &mut Doc<'_>) {
    let changelog = doc.view.volatile("changelog.txt");
    if !changelog.trim().is_empty() {
        doc.heading(2, "Recent changes (newest first)");
        doc.code(&changelog, 150);
    }
}

pub(super) fn old_notes(doc: &mut Doc<'_>) {
    let left: Vec<(String, String)> =
        doc.notes.iter().filter(|(heading, _)| !doc.used_notes.contains(heading)).cloned().collect();
    if left.is_empty() {
        return;
    }
    doc.heading(2, "Notes about things no longer present");
    for (heading, body) in left {
        doc.heading(3, &heading);
        doc.line(body);
    }
}
