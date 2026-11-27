//! Docker, Incus and monitoring sections of the context file.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{Doc, clip, shown};
use crate::redact::{env_entry, mask_token};

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(str::to_string).collect()
}

/// Environment the image sets for itself: noise in a description of the server.
const IMAGE_ENV: [&str; 16] = [
    "PATH=",
    "HOME=",
    "LANG=",
    "LC_ALL=",
    "TERM=",
    "HOSTNAME=",
    "S6_",
    "PYTHON",
    "GPG_KEY=",
    "JAVA_HOME=",
    "NODE_VERSION=",
    "YARN_VERSION=",
    "GOLANG_VERSION=",
    "PHP_",
    "LSIO_",
    "XDG_",
];

/// Labels that configure other tools (proxies, updaters, dashboards).
const TOOL_LABELS: [&str; 13] = [
    "traefik.",
    "caddy",
    "homepage.",
    "com.centurylinklabs.watchtower",
    "diun.",
    "autoheal",
    "swag",
    "tsdproxy",
    "glance",
    "kuma",
    "wud.",
    "dockflare",
    "cloudflare",
];

fn container_block(doc: &mut Doc<'_>, c: &Value, states: &BTreeMap<String, String>) {
    let name = text(&c["name"]);
    let hc = &c["host_config"];
    let state = states.get(name).map_or("?", String::as_str);
    let restart = match text(&hc["RestartPolicy"]["Name"]) {
        "" => "no",
        policy => policy,
    };
    doc.line(format!("- **{name}** — `{}` · {state} · restart: {restart}", text(&c["image"])));
    if state != "running" && state != "?" && ["always", "unless-stopped"].contains(&restart) {
        doc.warn(format!("container {name} is {state} although its restart policy is {restart}"));
    }
    let mut ports = Vec::new();
    for (port, bindings) in hc["PortBindings"].as_object().into_iter().flatten() {
        for b in bindings.as_array().into_iter().flatten() {
            let ip = text(&b["HostIp"]);
            let prefix = if ip.is_empty() || ip == "0.0.0.0" || ip == "::" { String::new() } else { format!("{ip}:") };
            let entry = format!("{prefix}{}→{port}", text(&b["HostPort"]));
            if !ports.contains(&entry) {
                ports.push(entry);
            }
        }
    }
    if !ports.is_empty() {
        doc.line(format!("  - ports: {}", ports.join(", ")));
    }
    let mode = text(&hc["NetworkMode"]);
    if mode == "host" || mode == "none" || mode.starts_with("container:") {
        doc.line(format!("  - network_mode: {mode}"));
    } else if let Some(networks) = c["networks"].as_object() {
        let described: Vec<String> = networks
            .iter()
            .map(|(network, n)| {
                let aliases: Vec<String> = strings(&n["aliases"]).into_iter().filter(|a| a != name).collect();
                let mut details = Vec::new();
                if !text(&n["ipv4"]).is_empty() {
                    details.push(format!("ip {}", text(&n["ipv4"])));
                }
                if !aliases.is_empty() {
                    details.push(format!("aliases {}", aliases.join(",")));
                }
                if details.is_empty() { network.clone() } else { format!("{network} ({})", details.join(", ")) }
            })
            .collect();
        if !described.is_empty() {
            doc.line(format!("  - networks: {}", described.join(", ")));
        }
    }
    let mounts: Vec<String> = c["mounts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let source = match (text(&m["type"]), text(&m["name"])) {
                ("bind", _) => text(&m["source"]).to_string(),
                (_, volume) if !volume.is_empty() => format!("vol:{volume}"),
                _ if m["anonymous"] == true => "anon-volume".to_string(),
                (kind, _) => kind.to_string(),
            };
            format!("{source} → {}{}", text(&m["destination"]), if m["ro"] == true { " (ro)" } else { "" })
        })
        .collect();
    if !mounts.is_empty() {
        doc.line(format!("  - mounts: {}", mounts.join("; ")));
    }
    let env: Vec<String> = strings(&c["env"])
        .into_iter()
        .filter(|e| !IMAGE_ENV.iter().any(|p| e.starts_with(p)))
        .map(|e| shown(&env_entry(&e)))
        .collect();
    if !env.is_empty() {
        let more = if env.len() > 40 { format!(" … (+{})", env.len() - 40) } else { String::new() };
        doc.line(format!(
            "  - env: {}{more}",
            env.iter().take(40).map(|e| format!("`` {} ``", clip(e, 120))).collect::<Vec<_>>().join(", ")
        ));
    }
    let labels: Vec<String> = c["labels"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| TOOL_LABELS.iter().any(|p| key.starts_with(p)))
        .map(|(key, value)| format!("`` {} ``", clip(&shown(&format!("{key}={}", text(value))), 160)))
        .collect();
    if !labels.is_empty() {
        doc.line(format!("  - labels: {}", labels.iter().take(25).cloned().collect::<Vec<_>>().join(", ")));
    }
    let mut flags = Vec::new();
    if hc["Privileged"] == true {
        flags.push("privileged".to_string());
    }
    if !strings(&hc["CapAdd"]).is_empty() {
        flags.push(format!("cap_add {}", strings(&hc["CapAdd"]).join(",")));
    }
    let devices: Vec<&str> = hc["Devices"].as_array().into_iter().flatten().map(|d| text(&d["PathOnHost"])).collect();
    if !devices.is_empty() {
        flags.push(format!("devices {}", devices.join(",")));
    }
    if hc["DeviceRequests"].as_array().is_some_and(|r| !r.is_empty()) {
        flags.push("GPU".to_string());
    }
    if c["healthcheck"]["Test"].as_array().is_some_and(|t| t.first().is_some_and(|x| x != "NONE")) {
        flags.push("healthcheck".to_string());
    }
    if !text(&c["user"]).is_empty() {
        flags.push(format!("user {}", text(&c["user"])));
    }
    if !flags.is_empty() {
        doc.line(format!("  - {}", flags.join(" · ")));
    }
}

pub(super) fn docker(doc: &mut Doc<'_>) {
    let v = doc.view;
    let (containers, projects, networks, volumes, services) =
        (v.containers(), v.compose_projects(), v.networks(), v.volumes(), v.services());
    if containers.is_empty() && projects.is_empty() {
        return;
    }
    let states = v.container_states();
    doc.heading(2, "Docker");
    let running = containers.iter().filter(|c| states.get(text(&c["name"])).is_some_and(|s| s == "running")).count();
    doc.line(format!(
        "{} containers ({running} running), {} compose stacks, {} swarm services, {} networks, {} named volumes.",
        containers.len(),
        projects.len(),
        services.len(),
        networks.len(),
        volumes.len()
    ));

    if !networks.is_empty() {
        let mut attached: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for c in &containers {
            for network in c["networks"].as_object().into_iter().flatten().map(|(n, _)| n) {
                attached.entry(network.clone()).or_default().push(text(&c["name"]).to_string());
            }
        }
        for s in &services {
            for n in s["networks"].as_array().into_iter().flatten() {
                attached.entry(text(&n["name"]).to_string()).or_default().push(format!("svc:{}", text(&s["name"])));
            }
        }
        let rows: Vec<Vec<String>> = networks
            .iter()
            .map(|n| {
                let name = text(&n["name"]);
                let subnets: Vec<String> = n["subnets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| format!("{} gw {}", text(&s["subnet"]), text(&s["gateway"])))
                    .collect();
                let created_by = match (
                    text(&n["labels"]["com.docker.compose.project"]),
                    text(&n["labels"]["com.docker.stack.namespace"]),
                ) {
                    (project, _) if !project.is_empty() => format!("compose: {project}"),
                    (_, stack) if !stack.is_empty() => format!("swarm stack: {stack}"),
                    _ if ["bridge", "host", "none", "ingress", "docker_gwbridge"].contains(&name) => {
                        "built in".to_string()
                    }
                    _ => "manual / external".to_string(),
                };
                vec![
                    name.to_string(),
                    text(&n["driver"]).to_string(),
                    subnets.join(", "),
                    if n["internal"] == true { "yes" } else { "" }.to_string(),
                    created_by,
                    attached.get(name).map(|a| a.join(", ")).unwrap_or_default(),
                ]
            })
            .collect();
        doc.heading(3, "Networks");
        doc.table(&["Name", "Driver", "Subnet", "Internal", "Created by", "Attached"], &rows);
    }
    if !volumes.is_empty() {
        let mut users: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for c in &containers {
            for m in c["mounts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["type"] == "volume" && !text(&m["name"]).is_empty())
            {
                users.entry(text(&m["name"]).to_string()).or_default().push(text(&c["name"]).to_string());
            }
        }
        for s in &services {
            for m in s["mounts"].as_array().into_iter().flatten().filter(|m| m["type"] == "volume") {
                users.entry(text(&m["source"]).to_string()).or_default().push(format!("svc:{}", text(&s["name"])));
            }
        }
        let rows: Vec<Vec<String>> = volumes
            .iter()
            .map(|vol| {
                let name = text(&vol["name"]);
                vec![
                    name.to_string(),
                    text(&vol["driver"]).to_string(),
                    users.get(name).map_or_else(|| "(unused)".to_string(), |u| u.join(", ")),
                    text(&vol["mountpoint"]).to_string(),
                ]
            })
            .collect();
        doc.heading(3, "Named volumes");
        doc.table(&["Volume", "Driver", "Used by", "Mountpoint"], &rows);
    }

    if !projects.is_empty() {
        doc.heading(3, "Compose stacks");
        for p in &projects {
            let name = text(&p["project"]);
            doc.heading(4, &format!("Stack `{name}`"));
            let files = strings(&p["config_files"]).iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ");
            let env_files = strings(&p["env_files"]);
            let env = if env_files.is_empty() {
                String::new()
            } else {
                format!(" · env: {}", env_files.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", "))
            };
            doc.line(format!(
                "- dir: `{}` · files: {files}{env} · source: {}",
                text(&p["workdir"]),
                text(&p["source"])
            ));
            let (defined, created) = (strings(&p["defined_services"]), strings(&p["services"]));
            if !defined.is_empty() {
                let missing: Vec<&String> = defined.iter().filter(|s| !created.contains(s)).collect();
                let not_created = if !missing.is_empty() && !strings(&p["containers"]).is_empty() {
                    format!(" — **not created:** {}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
                } else {
                    String::new()
                };
                doc.line(format!("- services defined: {}{not_created}", defined.join(", ")));
            }
            doc.write_note(&format!("Stack: {name}"));
            for container in strings(&p["containers"]) {
                if let Some(c) = containers.iter().find(|c| c["name"] == container.as_str()) {
                    container_block(doc, c, &states);
                }
            }
        }
    }
    if !services.is_empty() {
        doc.heading(3, &format!("Swarm services ({})", services.len()));
        let stacks = v.lines("docker/stacks.txt");
        if !stacks.is_empty() {
            doc.line(format!(
                "Stacks: {}",
                stacks.iter().filter_map(|l| l.split('\t').next()).collect::<Vec<_>>().join(", ")
            ));
        }
        let mut rows = Vec::new();
        for s in &services {
            let name = text(&s["name"]);
            // running task containers are named <service>.<slot>.<task id>
            let running_tasks =
                states.iter().filter(|(c, state)| c.starts_with(&format!("{name}.")) && *state == "running").count();
            let wanted = s["replicas"].as_u64();
            if let Some(wanted) = wanted
                && (running_tasks as u64) < wanted
            {
                doc.warn(format!("swarm service {name} has {running_tasks} of {wanted} replicas running"));
            }
            rows.push(vec![
                name.to_string(),
                text(&s["image"]).to_string(),
                match wanted {
                    Some(w) => format!("{running_tasks}/{w}"),
                    None => text(&s["replicas"]).to_string(),
                },
                s["ports"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|p| format!("{}→{}", p["published"], p["target"]))
                    .collect::<Vec<_>>()
                    .join(", "),
                s["networks"].as_array().into_iter().flatten().map(|n| text(&n["name"])).collect::<Vec<_>>().join(", "),
                s["mounts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| format!("{} → {}", text(&m["source"]), text(&m["destination"])))
                    .collect::<Vec<_>>()
                    .join("; "),
            ]);
        }
        doc.table(&["Service", "Image", "Replicas", "Ports", "Networks", "Mounts"], &rows);
        for s in &services {
            let name = text(&s["name"]);
            let env: Vec<String> = strings(&s["env"]).into_iter().map(|e| shown(&env_entry(&e))).collect();
            let note = doc.note(&format!("Service: {name}"));
            if !env.is_empty() || note.is_some() {
                doc.line(format!(
                    "- **{name}**{}",
                    if env.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " — env: {}",
                            env.iter()
                                .take(40)
                                .map(|e| format!("`` {} ``", clip(e, 120)))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                ));
                if let Some(note) = note {
                    doc.line(note);
                }
            }
        }
    }
    let standalone: Vec<&Value> =
        containers.iter().filter(|c| c["labels"].get("com.docker.compose.project").is_none()).collect();
    if !standalone.is_empty() {
        doc.heading(
            3,
            &format!(
                "Standalone containers (not in compose; recreate scripts in `{}`)",
                v.state().snapshot().join("docker/standalone").display()
            ),
        );
        for c in standalone {
            container_block(doc, c, &states);
            doc.write_note(&format!("Container: {}", text(&c["name"])));
        }
    }
}

pub(super) fn incus(doc: &mut Doc<'_>) {
    let v = doc.view;
    let instances = v.incus_instances();
    if instances.is_empty() && v.text("incus/preseed.yaml").is_empty() {
        return;
    }
    doc.heading(2, "VMs & system containers (Incus)");
    let list = v.volatile("incus-list.txt");
    if !list.trim().is_empty() {
        doc.code(&list, 40);
    }
    for name in instances {
        let config = v.text(&format!("incus/instances/{name}.yaml"));
        let limits: Vec<String> =
            config.lines().map(str::trim).filter(|l| l.starts_with("limits.")).map(|l| l.replace('"', "")).collect();
        let snapshots = v.lines(&format!("incus/instances/{name}.snapshots"));
        let limits = if limits.is_empty() { String::new() } else { format!(" ({})", limits.join(", ")) };
        doc.line(format!(
            "- **{name}**{limits} — snapshots: {}",
            if snapshots.is_empty() { "none".to_string() } else { snapshots.join(", ") }
        ));
        doc.write_note(&format!("Instance: {name}"));
    }
    if let Some((_, reason)) = doc
        .config
        .data
        .fungible
        .iter()
        .find(|p| crate::backup::under("/var/lib/incus", p))
        .map(|p| (p, doc.config.data.fungible_reason.get(p).cloned().unwrap_or_default()))
    {
        doc.line(format!(
            "_Instance disks are declared fungible{}: they are not backed up._",
            if reason.is_empty() { String::new() } else { format!(" ({reason})") }
        ));
    }
    doc.line(format!(
        "_Pools, networks and profiles are in `{}` (restorable with `incus admin init --preseed`)._",
        v.state().snapshot().join("incus/preseed.yaml").display()
    ));
}

/// `https://hc-ping.com/<key>/<slug>` → `https://hc-ping.com/… (key hidden)`.
fn hide_ping_key(url: &str) -> String {
    match url
        .split_once("://")
        .and_then(|(scheme, rest)| rest.split_once('/').map(|(host, _)| format!("{scheme}://{host}/… (key hidden)")))
    {
        Some(hidden) if url.contains("hc-ping") || url.contains("healthchecks") => hidden,
        _ => url.to_string(),
    }
}

pub(super) fn monitoring(doc: &mut Doc<'_>) {
    let v = doc.view;
    let kuma = v.json("monitoring/kuma-monitors.json");
    let monitors: Vec<Value> = kuma["monitors"].as_array().cloned().unwrap_or_default();
    let integrations = v.list("monitoring/integrations.json");
    if monitors.is_empty() && integrations.is_empty() {
        return;
    }
    let last: Value = serde_json::from_str(&v.volatile("kuma-last.json")).unwrap_or(Value::Null);
    doc.heading(2, "Monitoring & alerting (existing: adopt, never recreate)");
    doc.line("_Uptime Kuma's monitors and what feeds them. Anything that automates monitors must match these by name and push token and keep them. Push tokens are masked here; the full values are in the snapshot._");
    let feeder = |i: &Value| {
        if i["file"].is_string() {
            text(&i["file"]).to_string()
        } else {
            format!("container {}", text(&i["container"]))
        }
    };
    // which moi check reports to which monitor, by configured name
    let checks = &doc.config.checks;
    let mut owners: Vec<(String, String)> = Vec::new();
    for (check, monitor, enabled) in [
        ("resources", &checks.resources.common.monitor, checks.resources.common.enabled),
        ("storage", &checks.storage.common.monitor, checks.storage.common.enabled),
        ("services", &checks.services.common.monitor, checks.services.common.enabled),
        ("backup", &checks.backup.monitor, doc.config.backup.enabled),
    ] {
        if let (Some(m), true) = (monitor, enabled) {
            owners.push((m.clone(), format!("moi check `{check}`")));
        }
    }
    for custom in checks.custom.iter().filter(|c| c.common.enabled) {
        if let Some(m) = &custom.common.monitor {
            owners.push((m.clone(), format!("moi check `{}`", custom.name)));
        }
    }

    if !monitors.is_empty() {
        let mut rows = Vec::new();
        for m in &monitors {
            let name = text(&m["name"]);
            let kind = text(&m["type"]);
            let target = match kind {
                "push" => format!("push {}", mask_token(text(&m["push_token"]))),
                "dns" | "ping" | "tailscale-ping" => text(&m["hostname"]).to_string(),
                "port" => format!("{}:{}", text(&m["hostname"]), m["port"]),
                "docker" => format!("container {}", text(&m["docker_container"])),
                _ => hide_ping_key(text(&m["url"])),
            };
            let mut fed_by: Vec<String> = integrations
                .iter()
                .filter(|i| !text(&m["push_token"]).is_empty() && i["token"] == m["push_token"])
                .map(|i| format!("{}{}", feeder(i), if i["stale_copy"] == true { " (stale copy)" } else { "" }))
                .collect();
            fed_by.extend(owners.iter().filter(|(monitor, _)| monitor == name).map(|(_, owner)| owner.clone()));
            fed_by.sort();
            fed_by.dedup();
            let live_feeders = fed_by.iter().filter(|f| !f.ends_with("(stale copy)")).count();
            if kind == "push" && m["active"] != 0 && live_feeders == 0 {
                fed_by.push("⚠ nothing on this server pushes to it".to_string());
                doc.warn(format!("push monitor \"{name}\" is fed by nothing on this server"));
            }
            let message = text(&last[name]["msg"]);
            if message.contains("[object Object]") {
                doc.warn(format!("Kuma monitor \"{name}\" last received a malformed message ([object Object]): its feeder sends an object where text belongs"));
            }
            let last_result = if last[name].is_object() {
                format!("{}: {}", text(&last[name]["status"]), clip(&shown(message), 120))
            } else {
                String::new()
            };
            rows.push(vec![
                name.to_string(),
                kind.to_string(),
                target,
                m["interval"].to_string(),
                if m["active"] == 0 { "paused" } else { "yes" }.to_string(),
                text(&m["group"]).to_string(),
                strings(&m["notifications"]).join(", "),
                fed_by.join(", "),
                last_result,
            ]);
        }
        doc.line(format!("Kuma database: `{}` — {} monitors", text(&kuma["db"]), monitors.len()));
        doc.table(
            &["Monitor", "Type", "Target", "Every (s)", "Active", "Group", "Notifications", "Fed by", "Last result"],
            &rows,
        );
        let watchdogs: Vec<String> = monitors
            .iter()
            .filter(|m| text(&m["url"]).contains("hc-ping") || text(&m["url"]).contains("healthchecks"))
            .map(|m| format!("{} → {}", text(&m["name"]), hide_ping_key(text(&m["url"]))))
            .collect();
        if !watchdogs.is_empty() {
            doc.line(format!("**Watchdog:** {} — Kuma pings healthchecks.io, so healthchecks.io alerts when Kuma (or this server) dies. moi never edits this monitor.", watchdogs.join(", ")));
        }
    }
    let known: Vec<&str> = monitors.iter().map(|m| text(&m["push_token"])).filter(|t| !t.is_empty()).collect();
    let rows: Vec<Vec<String>> = integrations
        .iter()
        .map(|i| {
            let called_by = strings(&i["called_by"]).join("; ");
            vec![
                feeder(i),
                text(&i["kind"]).to_string(),
                mask_token(text(&i["token"])),
                if i["container"].is_string() {
                    format!("env {}", text(&i["env"]))
                } else if called_by.is_empty() {
                    "(not scheduled by cron or systemd)".to_string()
                } else {
                    clip(&shown(&called_by), 200)
                },
            ]
        })
        .collect();
    if !rows.is_empty() {
        doc.blank();
        doc.line("**What reports out:**");
        doc.table(&["File or container", "Reports to", "Token", "Scheduled by"], &rows);
    }
    for i in &integrations {
        let token = text(&i["token"]);
        if i["stale_copy"] == true {
            doc.warn(format!(
                "{} is a stale copy that still holds a push token ({}): delete it",
                text(&i["file"]),
                mask_token(token)
            ));
        } else if i["kind"] == "uptime-kuma-push" && !monitors.is_empty() && !known.contains(&token) {
            doc.warn(format!("{} pushes to a Kuma token ({}) that matches no monitor", feeder(i), mask_token(token)));
        }
    }
}
