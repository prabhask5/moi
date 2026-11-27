//! Docker: containers, networks, volumes, swarm services, compose projects, and run scripts for
//! containers that belong to no compose project. Only fields that stay the same until someone changes
//! the configuration are kept (docs/spec/02-snapshot.md § Docker normalization).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::collect::{DATA_SUFFIXES, LIGHT_PRUNE, lines};
use super::{Collector, Ctx, Output, Walk, walk_files};
use crate::docker::DEFAULT_SOCKET;
use crate::host::{FileKind, Host};

pub(crate) struct Docker;

const HOST_CONFIG_KEYS: [&str; 32] = [
    "PortBindings",
    "RestartPolicy",
    "NetworkMode",
    "Privileged",
    "CapAdd",
    "CapDrop",
    "Devices",
    "DeviceRequests",
    "ExtraHosts",
    "Dns",
    "DnsSearch",
    "LogConfig",
    "Memory",
    "MemoryReservation",
    "NanoCpus",
    "CpuShares",
    "CpusetCpus",
    "ShmSize",
    "Tmpfs",
    "Sysctls",
    "SecurityOpt",
    "PidMode",
    "IpcMode",
    "UsernsMode",
    "VolumesFrom",
    "GroupAdd",
    "Runtime",
    "Init",
    "Ulimits",
    "ReadonlyRootfs",
    "OomKillDisable",
    "PidsLimit",
];

const COMPOSE_NAMES: [&str; 4] = ["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Null, false, 0, "", [] and {} carry no information in a normalized record.
fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// An object without its empty members.
fn compact(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        if !is_empty(&value) {
            map.insert(key.to_string(), value);
        }
    }
    Value::Object(map)
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn sorted_strings(v: &Value) -> Vec<String> {
    let mut items: Vec<String> =
        v.as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(str::to_string).collect();
    items.sort();
    items
}

/// Pretty JSON with a final newline; empty for an empty list (no file).
fn dump(items: &[Value]) -> String {
    if items.is_empty() {
        return String::new();
    }
    format!("{}\n", serde_json::to_string_pretty(items).unwrap_or_default())
}

/// Containers reduced to their configuration, sorted by name. Swarm task containers are left out: their
/// names change with every deployment and the service describes them.
pub(crate) fn normalize_containers(raw: &[Value]) -> Vec<Value> {
    let names: BTreeMap<&str, &str> =
        raw.iter().map(|c| (text(&c["Id"]), text(&c["Name"]).trim_start_matches('/'))).collect();
    let mut out = Vec::new();
    for c in raw {
        let (cfg, hc) = (&c["Config"], &c["HostConfig"]);
        if cfg["Labels"].get("com.docker.swarm.task.id").is_some() {
            continue;
        }
        let id = text(&c["Id"]);
        let short = id.get(..12).unwrap_or(id);

        let mut networks = Map::new();
        for (name, n) in c["NetworkSettings"]["Networks"].as_object().into_iter().flatten() {
            let aliases: BTreeSet<String> = n["Aliases"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str())
                .filter(|a| *a != short && !is_hex(a, 12))
                .map(str::to_string)
                .collect();
            let ipam = &n["IPAMConfig"];
            networks.insert(
                name.clone(),
                compact(vec![
                    ("aliases", json!(aliases)),
                    ("ipv4", ipam["IPv4Address"].clone()),
                    ("ipv6", ipam["IPv6Address"].clone()),
                ]),
            );
        }

        let mut mounts: Vec<Value> = c["Mounts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| {
                let name = text(&m["Name"]);
                let anonymous = is_hex(name, 64);
                let driver = text(&m["Driver"]);
                compact(vec![
                    ("type", m["Type"].clone()),
                    ("name", if anonymous { Value::Null } else { json!(name) }),
                    ("anonymous", json!(anonymous)),
                    ("source", if anonymous { Value::Null } else { m["Source"].clone() }),
                    ("destination", m["Destination"].clone()),
                    ("ro", json!(m["RW"] == false)),
                    ("driver", if driver == "local" { Value::Null } else { json!(driver) }),
                ])
            })
            .collect();
        mounts.sort_by(|a, b| text(&a["destination"]).cmp(text(&b["destination"])));

        let mut host_config = Map::new();
        for key in HOST_CONFIG_KEYS {
            if !is_empty(&hc[key]) {
                host_config.insert(key.to_string(), hc[key].clone());
            }
        }
        // the id of the container whose network is shared changes when that container is recreated
        if let Some(other) = text(&hc["NetworkMode"]).strip_prefix("container:") {
            let name =
                names.iter().find(|(id, _)| !other.is_empty() && id.starts_with(other)).map_or(other, |(_, name)| name);
            host_config.insert("NetworkMode".to_string(), json!(format!("container:{name}")));
        }

        let hostname = text(&cfg["Hostname"]);
        let mut exposed: Vec<&String> = cfg["ExposedPorts"].as_object().map(|o| o.keys().collect()).unwrap_or_default();
        exposed.sort();
        out.push(compact(vec![
            ("name", json!(text(&c["Name"]).trim_start_matches('/'))),
            ("image", cfg["Image"].clone()),
            ("image_id", c["Image"].clone()),
            ("cmd", cfg["Cmd"].clone()),
            ("entrypoint", cfg["Entrypoint"].clone()),
            ("env", json!(sorted_strings(&cfg["Env"]))),
            ("labels", cfg["Labels"].clone()),
            ("user", cfg["User"].clone()),
            ("hostname", if is_hex(hostname, 12) { Value::Null } else { json!(hostname) }),
            ("healthcheck", cfg["Healthcheck"].clone()),
            ("exposed_ports", json!(exposed)),
            ("host_config", Value::Object(host_config)),
            ("mounts", json!(mounts)),
            ("networks", Value::Object(networks)),
        ]));
    }
    out.sort_by(|a, b| text(&a["name"]).cmp(text(&b["name"])));
    out
}

pub(crate) fn normalize_networks(raw: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = raw
        .iter()
        .map(|n| {
            let ipam = &n["IPAM"];
            let subnets: Vec<Value> = ipam["Config"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| {
                    compact(vec![
                        ("subnet", x["Subnet"].clone()),
                        ("gateway", x["Gateway"].clone()),
                        ("ip_range", x["IPRange"].clone()),
                        ("aux", x["AuxiliaryAddresses"].clone()),
                    ])
                })
                .collect();
            let ipam_driver = text(&ipam["Driver"]);
            compact(vec![
                ("name", n["Name"].clone()),
                ("id", n["Id"].clone()),
                ("driver", n["Driver"].clone()),
                ("scope", n["Scope"].clone()),
                ("internal", n["Internal"].clone()),
                ("attachable", n["Attachable"].clone()),
                ("ipv6", n["EnableIPv6"].clone()),
                ("subnets", json!(subnets)),
                ("ipam_driver", if ipam_driver == "default" { Value::Null } else { json!(ipam_driver) }),
                ("ipam_options", ipam["Options"].clone()),
                ("options", n["Options"].clone()),
                ("labels", n["Labels"].clone()),
                ("config_only", n["ConfigOnly"].clone()),
                ("config_from", n["ConfigFrom"]["Network"].clone()),
            ])
        })
        .collect();
    out.sort_by(|a, b| text(&a["name"]).cmp(text(&b["name"])));
    out
}

/// Named volumes only: anonymous ones are recreated with their container.
pub(crate) fn normalize_volumes(raw: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = raw
        .iter()
        .filter(|v| !is_hex(text(&v["Name"]), 64))
        .map(|v| {
            compact(vec![
                ("name", v["Name"].clone()),
                ("driver", v["Driver"].clone()),
                ("mountpoint", v["Mountpoint"].clone()),
                ("labels", v["Labels"].clone()),
                ("options", v["Options"].clone()),
            ])
        })
        .collect();
    out.sort_by(|a, b| text(&a["name"]).cmp(text(&b["name"])));
    out
}

/// Swarm services; network ids are replaced by names from the normalized networks.
pub(crate) fn normalize_services(raw: &[Value], networks: &[Value]) -> Vec<Value> {
    let net_name =
        |id: &Value| networks.iter().find(|n| n["id"] == *id).map_or_else(|| id.clone(), |n| n["name"].clone());
    let mut out: Vec<Value> = raw
        .iter()
        .map(|service| {
            let spec = &service["Spec"];
            let task = &spec["TaskTemplate"];
            let cs = &task["ContainerSpec"];
            let (image, digest) = text(&cs["Image"]).split_once('@').unwrap_or((text(&cs["Image"]), ""));
            let replicas = if spec["Mode"].get("Global").is_some() {
                json!("global")
            } else {
                spec["Mode"]["Replicated"]["Replicas"].clone()
            };
            let nets = if is_empty(&task["Networks"]) { &spec["Networks"] } else { &task["Networks"] };
            let list = |v: &Value, f: &dyn Fn(&Value) -> Value| -> Value {
                json!(v.as_array().into_iter().flatten().map(f).collect::<Vec<_>>())
            };
            compact(vec![
                ("name", spec["Name"].clone()),
                ("image", json!(image)),
                ("image_digest", json!(digest)),
                ("replicas", replicas),
                ("env", json!(sorted_strings(&cs["Env"]))),
                ("labels", spec["Labels"].clone()),
                ("container_labels", cs["Labels"].clone()),
                ("command", cs["Command"].clone()),
                ("args", cs["Args"].clone()),
                ("hostname", cs["Hostname"].clone()),
                (
                    "mounts",
                    list(&cs["Mounts"], &|m| {
                        compact(vec![
                            ("type", m["Type"].clone()),
                            ("source", m["Source"].clone()),
                            ("destination", m["Target"].clone()),
                            ("ro", m["ReadOnly"].clone()),
                        ])
                    }),
                ),
                (
                    "ports",
                    list(&spec["EndpointSpec"]["Ports"], &|p| {
                        compact(vec![
                            ("published", p["PublishedPort"].clone()),
                            ("target", p["TargetPort"].clone()),
                            ("protocol", p["Protocol"].clone()),
                            ("mode", p["PublishMode"].clone()),
                        ])
                    }),
                ),
                (
                    "networks",
                    list(nets, &|n| compact(vec![("name", net_name(&n["Target"])), ("aliases", n["Aliases"].clone())])),
                ),
                ("constraints", task["Placement"]["Constraints"].clone()),
                ("restart", task["RestartPolicy"]["Condition"].clone()),
                ("secrets", list(&cs["Secrets"], &|s| s["SecretName"].clone())),
                ("configs", list(&cs["Configs"], &|s| s["ConfigName"].clone())),
            ])
        })
        .collect();
    out.sort_by(|a, b| text(&a["name"]).cmp(text(&b["name"])));
    out
}

// ---------------------------------------------------------------------------------------------- compose

/// Top-level service names of a compose file, without a YAML parser: the keys one indentation level
/// below `services:`.
pub(crate) fn compose_services(yaml: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut inside = false;
    let mut indent = None;
    for line in yaml.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.trim_end() == "services:" {
            inside = true;
            indent = None;
            continue;
        }
        if !inside {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            break;
        }
        let current = line.len() - line.trim_start().len();
        if *indent.get_or_insert(current) != current {
            continue;
        }
        if let Some((key, _)) = line.trim().split_once(':') {
            let key = key.trim_matches(['"', '\'']);
            if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
                names.push(key.to_string());
            }
        }
    }
    names
}

fn project_name(dir_name: &str) -> String {
    let name: String =
        dir_name.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    if name.is_empty() { "project".to_string() } else { name }
}

/// Compose files lying under the search paths (four levels deep, never inside data directories).
fn compose_files_on_disk(cx: &Ctx) -> Vec<PathBuf> {
    let mut prune: Vec<String> = LIGHT_PRUNE.iter().map(ToString::to_string).collect();
    prune.extend(["overlay2", "volumes", "pgdata", "postgres", "mysql"].map(String::from));
    let walk = Walk { max_depth: 4, prune_names: prune, ..Walk::default() };
    let mut found = BTreeSet::new();
    for root in &cx.config.capture.compose_search {
        for file in walk_files(cx.host.as_ref(), Path::new(root), &walk) {
            if file.file_name().and_then(|n| n.to_str()).is_some_and(|n| COMPOSE_NAMES.contains(&n)) {
                found.insert(file);
            }
        }
    }
    found.into_iter().collect()
}

/// Compose projects from container labels (running or stopped) and from compose files on disk that no
/// container belongs to.
pub(crate) fn compose_projects(host: &dyn Host, containers: &[Value], on_disk: &[PathBuf]) -> Vec<Value> {
    // Portainer keeps its stacks at /data/compose/<id> inside its own volume: map that to the host.
    let portainer_data = containers
        .iter()
        .filter(|c| text(&c["image"]).contains("portainer"))
        .flat_map(|c| c["mounts"].as_array().into_iter().flatten())
        .find(|m| m["destination"] == "/data" && !text(&m["source"]).is_empty())
        .map(|m| text(&m["source"]).to_string());
    let host_path = |path: &str, workdir: Option<&str>| -> String {
        let path = path.trim();
        if path.is_empty() {
            return String::new();
        }
        let path = match workdir {
            Some(dir) if !path.starts_with('/') => format!("{}/{path}", dir.trim_end_matches('/')),
            _ => path.to_string(),
        };
        match &portainer_data {
            Some(data) if (path == "/data" || path.starts_with("/data/")) && !host.exists(Path::new(&path)) => {
                format!("{data}{}", &path["/data".len()..])
            }
            _ => path,
        }
    };

    #[derive(Default)]
    struct Project {
        workdir: String,
        config_files: Vec<String>,
        env_files: Vec<String>,
        services: BTreeSet<String>,
        containers: BTreeSet<String>,
        source: &'static str,
    }
    let mut projects: BTreeMap<String, Project> = BTreeMap::new();
    for c in containers {
        let label = |key: &str| text(&c["labels"][key]);
        let name = label("com.docker.compose.project");
        if name.is_empty() {
            continue;
        }
        let p =
            projects.entry(name.to_string()).or_insert_with(|| Project { source: "containers", ..Project::default() });
        let workdir = host_path(label("com.docker.compose.project.working_dir"), None);
        if !workdir.is_empty() {
            p.workdir = workdir;
        }
        let workdir = (!p.workdir.is_empty()).then(|| p.workdir.clone());
        for (key, list) in [
            ("com.docker.compose.project.config_files", &mut p.config_files),
            ("com.docker.compose.project.environment_file", &mut p.env_files),
        ] {
            for file in label(key).split(',') {
                let file = host_path(file, workdir.as_deref());
                if !file.is_empty() && !list.contains(&file) {
                    list.push(file);
                }
            }
        }
        if !label("com.docker.compose.service").is_empty() {
            p.services.insert(label("com.docker.compose.service").to_string());
        }
        p.containers.insert(text(&c["name"]).to_string());
    }
    let known_files: BTreeSet<String> = projects.values().flat_map(|p| p.config_files.clone()).collect();
    let mut known_dirs: BTreeSet<String> = projects.values().map(|p| p.workdir.clone()).collect();
    for file in on_disk {
        let file_text = file.display().to_string();
        let dir = file.parent().map(|d| d.display().to_string()).unwrap_or_default();
        if known_files.contains(&file_text) || known_dirs.contains(&dir) {
            continue;
        }
        let base = project_name(file.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()).unwrap_or_default());
        let mut name = base.clone();
        let mut n = 2;
        while projects.contains_key(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        known_dirs.insert(dir.clone());
        projects.insert(
            name,
            Project {
                workdir: dir,
                config_files: vec![file_text],
                source: "found on disk (no containers)",
                ..Project::default()
            },
        );
    }
    projects
        .into_iter()
        .map(|(name, p)| {
            let mut defined = Vec::new();
            for file in &p.config_files {
                for service in compose_services(&host.read_to_string(Path::new(file)).unwrap_or_default()) {
                    if !defined.contains(&service) {
                        defined.push(service);
                    }
                }
            }
            json!({
                "project": name,
                "workdir": if p.workdir.is_empty() { Value::Null } else { json!(p.workdir) },
                "config_files": p.config_files,
                "env_files": p.env_files,
                "services": p.services,
                "defined_services": defined,
                "containers": p.containers,
                "source": p.source,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------- standalone

/// Quote a word for `sh` when it needs it.
pub(crate) fn sh_quote(word: &str) -> String {
    let safe = !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c));
    if safe { word.to_string() } else { format!("'{}'", word.replace('\'', "'\"'\"'")) }
}

/// Labels images set themselves; repeating them in a run script is noise.
const IMAGE_LABEL_PREFIXES: [&str; 12] = [
    "org.opencontainers.",
    "org.label-schema.",
    "maintainer",
    "com.docker.",
    "build_version",
    "description",
    "vendor",
    "version",
    "name",
    "license",
    "url",
    "summary",
];

/// A `docker run` script that recreates a container which belongs to no compose project, built from its
/// normalized description. For a human to review — never executed by moi.
pub(crate) fn standalone_script(c: &Value) -> String {
    let name = text(&c["name"]);
    let hc = &c["host_config"];
    let strings = |v: &Value| -> Vec<String> {
        v.as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(str::to_string).collect()
    };
    let mut groups: Vec<Vec<String>> =
        vec![vec!["docker".into(), "run".into(), "-d".into()], vec!["--name".into(), name.into()]];
    let mut flag = |flag: &str, value: String| groups.push(vec![flag.to_string(), value]);

    let restart = text(&hc["RestartPolicy"]["Name"]);
    if !restart.is_empty() && restart != "no" {
        let retries = hc["RestartPolicy"]["MaximumRetryCount"].as_u64().unwrap_or(0);
        flag(
            "--restart",
            if restart == "on-failure" && retries > 0 { format!("{restart}:{retries}") } else { restart.to_string() },
        );
    }
    let mode = match text(&hc["NetworkMode"]) {
        "" => "default",
        mode => mode,
    };
    let shares_namespace = mode.starts_with("container:") || mode == "host" || mode == "none";
    let primary = (!shares_namespace && mode != "default" && mode != "bridge").then_some(mode);
    if shares_namespace || primary.is_some() {
        flag("--network", mode.to_string());
    }
    if let Some(primary) = primary {
        let net = &c["networks"][primary];
        for alias in strings(&net["aliases"]).into_iter().filter(|a| a != name) {
            flag("--network-alias", alias);
        }
        if !text(&net["ipv4"]).is_empty() {
            flag("--ip", text(&net["ipv4"]).to_string());
        }
    }
    for (option, key) in [("--hostname", "hostname"), ("--user", "user")] {
        if !text(&c[key]).is_empty() {
            flag(option, text(&c[key]).to_string());
        }
    }
    let mut published = BTreeSet::new();
    for (port, bindings) in hc["PortBindings"].as_object().into_iter().flatten() {
        for b in bindings.as_array().into_iter().flatten() {
            let (ip, host_port) = (text(&b["HostIp"]), text(&b["HostPort"]));
            let mut spec = String::new();
            if !ip.is_empty() && ip != "0.0.0.0" && ip != "::" {
                spec.push_str(&format!("{ip}:"));
            }
            if !host_port.is_empty() {
                spec.push_str(&format!("{host_port}:"));
            }
            spec.push_str(&port.replace("/tcp", ""));
            if published.insert(spec.clone()) {
                flag("-p", spec);
            }
        }
    }
    for m in c["mounts"].as_array().into_iter().flatten() {
        let ro = if m["ro"] == true { ":ro" } else { "" };
        let source = match text(&m["type"]) {
            "bind" => text(&m["source"]),
            "volume" => text(&m["name"]),
            _ => "",
        };
        if !source.is_empty() {
            flag("-v", format!("{source}:{}{ro}", text(&m["destination"])));
        }
    }
    for (path, options) in hc["Tmpfs"].as_object().into_iter().flatten() {
        let options = text(options);
        flag("--tmpfs", if options.is_empty() { path.clone() } else { format!("{path}:{options}") });
    }
    for env in strings(&c["env"]).into_iter().filter(|e| !e.starts_with("PATH=")) {
        flag("-e", env);
    }
    for (key, value) in c["labels"].as_object().into_iter().flatten() {
        if !IMAGE_LABEL_PREFIXES.iter().any(|p| key.starts_with(p)) {
            flag("--label", format!("{key}={}", text(value)));
        }
    }
    if hc["Privileged"] == true {
        groups.push(vec!["--privileged".into()]);
    }
    for (option, key) in [
        ("--cap-add", "CapAdd"),
        ("--cap-drop", "CapDrop"),
        ("--add-host", "ExtraHosts"),
        ("--dns", "Dns"),
        ("--security-opt", "SecurityOpt"),
        ("--group-add", "GroupAdd"),
    ] {
        for value in strings(&hc[key]) {
            groups.push(vec![option.to_string(), value]);
        }
    }
    for d in hc["Devices"].as_array().into_iter().flatten() {
        groups.push(vec!["--device".into(), format!("{}:{}", text(&d["PathOnHost"]), text(&d["PathInContainer"]))]);
    }
    if hc["DeviceRequests"].to_string().contains("gpu") {
        groups.push(vec!["--gpus".into(), "all".into()]);
    }
    let log = &hc["LogConfig"];
    if !text(&log["Type"]).is_empty() && log["Type"] != "json-file" {
        groups.push(vec!["--log-driver".into(), text(&log["Type"]).into()]);
    }
    for (key, value) in log["Config"].as_object().into_iter().flatten() {
        groups.push(vec!["--log-opt".into(), format!("{key}={}", text(value))]);
    }
    if let Some(memory) = hc["Memory"].as_u64().filter(|m| *m > 0) {
        groups.push(vec!["--memory".into(), memory.to_string()]);
    }
    if let Some(nano) = hc["NanoCpus"].as_f64().filter(|n| *n > 0.0) {
        groups.push(vec!["--cpus".into(), format!("{}", nano / 1e9)]);
    }
    if let Some(shm) = hc["ShmSize"].as_u64().filter(|s| *s > 0 && *s != 67_108_864) {
        groups.push(vec!["--shm-size".into(), shm.to_string()]);
    }
    for (key, value) in hc["Sysctls"].as_object().into_iter().flatten() {
        groups.push(vec!["--sysctl".into(), format!("{key}={}", text(value))]);
    }
    if !text(&hc["PidMode"]).is_empty() {
        groups.push(vec!["--pid".into(), text(&hc["PidMode"]).into()]);
    }
    let ipc = text(&hc["IpcMode"]);
    if !ipc.is_empty() && ipc != "private" && ipc != "shareable" {
        groups.push(vec!["--ipc".into(), ipc.into()]);
    }
    if hc["Init"] == true {
        groups.push(vec!["--init".into()]);
    }
    let runtime = text(&hc["Runtime"]);
    if !runtime.is_empty() && runtime != "runc" {
        groups.push(vec!["--runtime".into(), runtime.into()]);
    }
    for u in hc["Ulimits"].as_array().into_iter().flatten() {
        groups.push(vec!["--ulimit".into(), format!("{}={}:{}", text(&u["Name"]), u["Soft"], u["Hard"])]);
    }
    let mut args = strings(&c["cmd"]);
    let entrypoint = strings(&c["entrypoint"]);
    if let Some((first, rest)) = entrypoint.split_first() {
        groups.push(vec!["--entrypoint".into(), first.clone()]);
        args = rest.iter().cloned().chain(args).collect();
    }
    let mut last = vec![text(&c["image"]).to_string()];
    last.extend(args);
    groups.push(last);

    let body: Vec<String> =
        groups.iter().map(|g| g.iter().map(|w| sh_quote(w)).collect::<Vec<_>>().join(" ")).collect();
    let mut script = format!(
        "#!/bin/sh\n# Recreate standalone container \"{name}\": generated by moi from the Docker API.\n# Review before running. Better: move it into a compose file.\nset -e\n{}\n",
        body.join(" \\\n  ")
    );
    // networks beyond the first are attached after creation
    for (network, n) in c["networks"].as_object().into_iter().flatten() {
        if Some(network.as_str()) == primary || network == "bridge" || shares_namespace {
            continue;
        }
        let mut cmd = vec!["docker".to_string(), "network".into(), "connect".into()];
        for alias in strings(&n["aliases"]).into_iter().filter(|a| a != name) {
            cmd.extend(["--alias".to_string(), alias]);
        }
        if !text(&n["ipv4"]).is_empty() {
            cmd.extend(["--ip".to_string(), text(&n["ipv4"]).to_string()]);
        }
        cmd.extend([network.clone(), name.to_string()]);
        script.push_str(&cmd.iter().map(|w| sh_quote(w)).collect::<Vec<_>>().join(" "));
        script.push('\n');
    }
    script
}

// ---------------------------------------------------------------------------------------------- collector

impl Collector for Docker {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let mut out = Output::default();
        let installed = host.symlink_metadata(Path::new(DEFAULT_SOCKET)).is_ok() || cx.docker.ping();
        let mut containers = Vec::new();
        if installed {
            // A socket without an answering engine is a failure (previous state is kept), not "no Docker".
            let fail = |e: crate::docker::DockerError| e.to_string();
            let version = cx.docker.version().map_err(fail)?;
            let info = cx.docker.info().map_err(fail)?;
            let raw_containers = cx.docker.containers_inspect().map_err(fail)?;
            containers = normalize_containers(&raw_containers);
            let networks = normalize_networks(&cx.docker.networks().map_err(fail)?);
            out.stable("docker/version", format!("{}\n", version.version));
            out.stable("docker/root-dir", lines([text(&info["DockerRootDir"])].iter().filter(|s| !s.is_empty())));
            let compose_version = raw_containers
                .iter()
                .map(|c| text(&c["Config"]["Labels"]["com.docker.compose.version"]))
                .filter(|v| !v.is_empty())
                .max_by_key(|v| v.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>());
            out.stable("docker/compose-version", lines(compose_version));
            out.stable("docker/containers.json", dump(&containers));
            out.stable("docker/networks.json", dump(&networks));
            out.stable("docker/volumes.json", dump(&normalize_volumes(&cx.docker.volumes().map_err(fail)?)));

            let mut ps: Vec<String> = raw_containers
                .iter()
                .map(|c| {
                    format!(
                        "{}\t{}\t{}\t{}",
                        text(&c["Name"]).trim_start_matches('/'),
                        text(&c["State"]["Status"]),
                        text(&c["State"]["StartedAt"]),
                        text(&c["Config"]["Image"])
                    )
                })
                .collect();
            ps.sort();
            out.volatile("docker-ps.tsv", lines(ps));
            let mut images: Vec<String> = cx
                .docker
                .images()
                .unwrap_or_default()
                .iter()
                .map(|i| format!("{}\t{}\t{}", sorted_strings(&i["RepoTags"]).join(","), text(&i["Id"]), i["Size"]))
                .collect();
            images.sort();
            out.volatile("docker-images.tsv", lines(images));

            if info["Swarm"]["LocalNodeState"] == "active" {
                out.stable("docker/swarm-state", "active\n");
                let raw_services = cx.docker.services().map_err(fail)?;
                let services = normalize_services(&raw_services, &networks);
                out.stable("docker/services.json", dump(&services));
                let mut stacks: BTreeMap<String, usize> = BTreeMap::new();
                for s in &services {
                    let stack = text(&s["labels"]["com.docker.stack.namespace"]);
                    if !stack.is_empty() {
                        *stacks.entry(stack.to_string()).or_default() += 1;
                    }
                }
                out.stable("docker/stacks.txt", lines(stacks.iter().map(|(name, n)| format!("{name}\t{n}"))));
                let mut nodes: Vec<String> = cx
                    .docker
                    .nodes()
                    .unwrap_or_default()
                    .iter()
                    .map(|n| {
                        let manager = if n["ManagerStatus"]["Leader"] == true {
                            "Leader"
                        } else {
                            text(&n["ManagerStatus"]["Reachability"])
                        };
                        format!(
                            "{}\t{manager}\t{}",
                            text(&n["Description"]["Hostname"]),
                            text(&n["Spec"]["Availability"])
                        )
                    })
                    .collect();
                nodes.sort();
                out.stable("docker/swarm-nodes.txt", lines(nodes));
                for (file, list) in [
                    ("docker/swarm-secrets.txt", cx.docker.secrets()),
                    ("docker/swarm-configs.txt", cx.docker.configs()),
                ] {
                    let mut names: Vec<String> =
                        list.unwrap_or_default().iter().map(|x| text(&x["Spec"]["Name"]).to_string()).collect();
                    names.sort();
                    out.stable(file, lines(names));
                }
                let mut service_ls: Vec<String> = services
                    .iter()
                    .map(|s| {
                        let ports: Vec<String> = s["ports"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|p| format!("{}->{}", p["published"], p["target"]))
                            .collect();
                        format!("{}\t{}\t{}\t{}", text(&s["name"]), s["replicas"], text(&s["image"]), ports.join(","))
                    })
                    .collect();
                service_ls.sort();
                out.volatile("service-ls.tsv", lines(service_ls));
            }
        }

        // Compose projects exist on disk whether or not an engine runs.
        let projects = compose_projects(host, &containers, &compose_files_on_disk(cx));
        out.stable("docker/compose-projects.json", dump(&projects));
        let mut prune = cx.config.capture.prune_dirs.clone();
        prune.extend(LIGHT_PRUNE.iter().map(ToString::to_string));
        let tree = Walk {
            max_depth: 4,
            prune_names: prune,
            skip_suffixes: DATA_SUFFIXES.iter().map(ToString::to_string).collect(),
            limit: 2000,
            ..Walk::default()
        };
        for p in &projects {
            let workdir = text(&p["workdir"]);
            if !workdir.is_empty() && host.metadata(Path::new(workdir)).is_ok_and(|m| m.kind == FileKind::Dir) {
                out.grab_all(walk_files(host, Path::new(workdir), &tree));
            }
            for key in ["config_files", "env_files"] {
                out.grab_all(p[key].as_array().into_iter().flatten().filter_map(|f| f.as_str()).map(PathBuf::from));
            }
        }
        for c in containers.iter().filter(|c| c["labels"].get("com.docker.compose.project").is_none()) {
            let file: String = text(&c["name"])
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() || "_.-".contains(ch) { ch } else { '_' })
                .collect();
            out.stable(&format!("docker/standalone/{file}.sh"), standalone_script(c));
        }
        Ok(out)
    }
}
