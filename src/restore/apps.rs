//! The phases that bring the applications back: the Docker engine, the data from the backup, stacks
//! with their databases first, swarm services, Incus — and then moi itself, the firewall, the
//! monitors and the final verification.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use flate2::read::GzDecoder;
use serde_json::{Value, json};

use super::Run;
use crate::backup::offsite::Restic;
use crate::docker::{Docker, ExecOptions};
use crate::host::Cmd;
use crate::state::StateDir;

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn parsed(run: &Run<'_>, file: &str) -> Vec<Value> {
    serde_json::from_str::<Value>(&run.snapshot_file(file)).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

pub(super) fn docker_engine(run: &mut Run<'_>) {
    if !run.have("docker") {
        // what the old server used: the distribution's packages, or Docker's own
        let manual = run.snapshot_lines("packages/apt-manual.txt");
        if manual.iter().any(|p| p == "docker.io") {
            let compose = if manual.iter().any(|p| p == "docker-compose-v2") {
                "docker-compose-v2"
            } else {
                "docker-compose-plugin"
            };
            run.apt_install(&["docker.io", compose]);
        } else if manual.iter().any(|p| p == "docker-ce") {
            run.apt_install(&[
                "docker-ce",
                "docker-ce-cli",
                "containerd.io",
                "docker-buildx-plugin",
                "docker-compose-plugin",
            ]);
        } else if run.have("apt-get") {
            run.apt_install(&["docker.io", "docker-compose-v2"]);
        } else {
            run.warn("Docker is not installed and this machine has no apt: install Docker, then run `moi restore --from docker-engine` again");
        }
    }
    run.restore_path("/etc/docker", false);
    run.restore_path("/etc/systemd/system/docker.service.d", false);
    // a data root on another disk must exist before the engine starts
    let daemon: Value = run
        .host
        .read(&run.files.join("etc/docker/daemon.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or(Value::Null);
    if let Some(root) = daemon["data-root"].as_str().filter(|r| !r.is_empty()) {
        run.act(&["mkdir", "-p", root]);
    }
    run.act(&["systemctl", "daemon-reload"]);
    run.act(&["systemctl", "enable", "docker"]);
    if !run.act(&["systemctl", "restart", "docker"]) {
        run.warn("Docker did not start: journalctl -u docker -b");
    }
}

pub(super) fn data(run: &mut Run<'_>) {
    let Some(old) = &run.old else {
        run.warn("the bundle's moi.toml does not load, so moi does not know where the backup is: restore the data yourself, then go on with --from config");
        return;
    };
    let config = old.config.clone();
    if !config.backup.offsite.enabled {
        run.warn("the old server had no off-site backup configured: there is nothing to restore the data from. Copy it over yourself, then go on with --from config");
        return;
    }
    run.say("  The data comes back from the backup repository (the bundle holds none of it):");
    for path in &config.backup.paths {
        run.info(path);
    }
    for path in &config.data.fungible {
        let why = config.data.fungible_reason.get(path).cloned().unwrap_or_else(|| "declared replaceable".to_string());
        run.info(&format!("not in the backup: {path} — {why}"));
    }
    let restic = match Restic::new(run.host, &config.backup.offsite) {
        Ok(restic) => restic,
        Err(e) => {
            run.warn(&format!("cannot reach the repository: {e}"));
            return;
        }
    };
    // which snapshot: the one asked for, or the newest of the nightly ones
    let tag = config.backup.offsite.tag.clone();
    let snapshot = match &run.options.snapshot {
        Some(id) => id.clone(),
        None => {
            let listed = restic.run(&["snapshots", "--tag", &tag, "--json"], Some(Duration::from_secs(600)));
            let snapshots: Vec<Value> = listed
                .ok()
                .filter(|o| o.status == 0)
                .and_then(|o| serde_json::from_slice(&o.stdout).ok())
                .unwrap_or_default();
            match snapshots.iter().max_by_key(|s| text(&s["time"]).to_string()) {
                Some(latest) => {
                    run.info(&format!(
                        "latest `{tag}` snapshot: {} from {}",
                        text(&latest["short_id"]),
                        text(&latest["time"])
                    ));
                    text(&latest["id"]).to_string()
                }
                None if run.dry() => {
                    run.info(&format!("the repository does not answer from here yet (restic comes with the preflight phase): the latest `{tag}` snapshot would be used"));
                    "latest".to_string()
                }
                None => {
                    run.warn(&format!("the repository cannot be read or holds no `{tag}` snapshot: check the repository URL, password and keys in the bundle's secrets.toml"));
                    return;
                }
            }
        }
    };
    if !run.ask("Restore the data to where it was (files that exist are overwritten by the backup's)?", true) {
        return;
    }
    if run.dry() {
        run.say(&format!("    [dry-run] restic restore {snapshot} --target /"));
    } else {
        match restic.run(&["restore", &snapshot, "--target", "/"], None) {
            Ok(out) if out.status == 0 => run.ok("data restored"),
            Ok(out) => run.warn(&format!(
                "restic restore ended with status {}: {}",
                out.status,
                out.stderr_str().lines().last().unwrap_or_default().trim()
            )),
            Err(e) => run.warn(&format!("cannot run restic: {e}")),
        }
    }
    // what was never backed up gets its place back, empty
    for path in &config.data.fungible {
        if !run.host.exists(Path::new(path)) {
            run.act(&["mkdir", "-p", path]);
        }
    }
}

pub(super) fn incus(run: &mut Run<'_>) {
    let preseed = run.snapshot_file("incus/preseed.yaml");
    if preseed.trim().is_empty() {
        run.info("the old server had no Incus");
        return;
    }
    if !run.have("incus") {
        run.apt_install(&["incus"]);
    }
    // storage pools, networks and profiles as they were
    let init = Cmd::new("incus").args(["admin", "init", "--preseed"]).stdin(preseed.into_bytes());
    if !run.act_cmd(init) {
        run.warn("incus admin init failed: the preseed is in the bundle under snapshot/incus/preseed.yaml");
    }
    let fungible = run
        .old
        .as_ref()
        .map(|l| (l.config.data.fungible.clone(), l.config.data.fungible_reason.clone()))
        .unwrap_or_default();
    let note = fungible
        .0
        .iter()
        .find(|p| p.contains("incus"))
        .map(|p| fungible.1.get(p).cloned().unwrap_or_else(|| "declared replaceable".to_string()));
    for file in run.host.read_dir(&run.snap.join("incus/instances")).unwrap_or_default() {
        let Some(name) = file.strip_suffix(".yaml") else { continue };
        match &note {
            // its disk was never backed up, on purpose: say how it comes back
            Some(note) => run.info(&format!("instance {name}: not restored — {note}")),
            None => run.warn(&format!("instance {name}: its configuration is in the bundle (snapshot/incus/instances/{name}.yaml); its disk must come from your own backup of the storage pool")),
        }
    }
}

/// A database among the old containers or services: how to reach it and where its data lives.
struct Database {
    /// Container name (or, for a service, the service name).
    name: String,
    /// The compose service it is, if it is one.
    service: String,
    engine: &'static str,
    user: String,
    env: Vec<String>,
    /// Host directories or volume names holding its raw data.
    data: Vec<(String, String)>,
}

fn database(entry: &Value) -> Option<Database> {
    let engine = crate::backup::database_engine(text(&entry["image"]), &entry["env"])?;
    let env_of = |key: &str| {
        entry["env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find_map(|e| e.strip_prefix(&format!("{key}=")).map(str::to_string))
    };
    let (user, env) = if engine == "postgres" {
        (
            env_of("POSTGRES_USER").unwrap_or_else(|| "postgres".to_string()),
            env_of("POSTGRES_PASSWORD").map(|p| format!("PGPASSWORD={p}")).into_iter().collect(),
        )
    } else {
        (
            "root".to_string(),
            env_of("MARIADB_ROOT_PASSWORD")
                .or_else(|| env_of("MYSQL_ROOT_PASSWORD"))
                .map(|p| format!("MYSQL_PWD={p}"))
                .into_iter()
                .collect(),
        )
    };
    let data = entry["mounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            ["/var/lib/postgresql", "/var/lib/mysql", "/var/lib/mariadb"]
                .iter()
                .any(|d| crate::backup::under(text(&m["destination"]), d))
        })
        .map(|m| {
            (
                text(&m["type"]).to_string(),
                if text(&m["type"]) == "volume" && !text(&m["name"]).is_empty() {
                    text(&m["name"]).to_string()
                } else {
                    text(&m["source"]).to_string()
                },
            )
        })
        .collect();
    Some(Database {
        name: text(&entry["name"]).to_string(),
        service: text(&entry["labels"]["com.docker.compose.service"]).to_string(),
        engine,
        user,
        env,
        data,
    })
}

impl Run<'_> {
    /// Whether a database's data directory holds nothing yet (so its dump is to be loaded).
    fn data_is_empty(&self, docker: &Docker, db: &Database) -> bool {
        db.data.iter().all(|(kind, source)| match kind.as_str() {
            "volume" => !docker.volumes().unwrap_or_default().iter().any(|v| v["Name"] == source.as_str()),
            _ => self.host.read_dir(Path::new(source)).map_or(true, |names| names.is_empty()),
        })
    }

    /// The newest dump of a database that the data phase brought back.
    fn dump_of(&self, name: &str) -> Option<String> {
        let local = self.old.as_ref()?.config.backup.local_dir.clone()?;
        let run = crate::backup::drill::latest_run(self.host.as_ref(), &local)?;
        let file = format!("{run}/db/{name}.sql.gz");
        self.host.exists(Path::new(&file)).then_some(file)
    }

    /// Wait until a container runs (and, if it has a health check, is healthy).
    fn wait_for(&self, docker: &Docker, container: &str, seconds: u64) -> bool {
        let ready = || {
            docker.container(container).is_ok_and(|c| match c["State"]["Health"]["Status"].as_str() {
                Some(health) => health == "healthy",
                None => c["State"]["Running"].as_bool().unwrap_or(false),
            })
        };
        let mut waited = 0;
        while !ready() && waited < seconds {
            self.host.sleep(Duration::from_secs(2));
            waited += 2;
        }
        ready()
    }

    /// Load a database's dump into its fresh, empty instance.
    fn import(&mut self, docker: &Docker, db: &Database, container: &str) {
        if self.dry() {
            self.say(&format!(
                "    [dry-run] {}: if its data directory is empty, load its newest dump (<backup>/db/{}.sql.gz)",
                db.name, db.name
            ));
            return;
        }
        let Some(dump) = self.dump_of(&db.name) else {
            self.warn(&format!(
                "{}: its data directory is empty and no dump of it was restored: it starts empty",
                db.name
            ));
            return;
        };
        // accepting connections is more than running
        let ready: Vec<&str> = if db.engine == "postgres" {
            vec!["pg_isready", "-q", "-U", &db.user]
        } else {
            // MariaDB's images no longer carry the `mysql*` names: whichever client the image has
            vec![
                "sh",
                "-c",
                "if command -v mariadb-admin >/dev/null 2>&1; then exec mariadb-admin ping --silent; else exec mysqladmin ping --silent; fi",
            ]
        };
        let options = ExecOptions { env: db.env.clone(), ..ExecOptions::default() };
        let mut waited = 0;
        while !docker.exec(container, &ready, &options).is_ok_and(|o| o.success()) && waited < 120 {
            self.host.sleep(Duration::from_secs(2));
            waited += 2;
        }
        let client: Vec<&str> = if db.engine == "postgres" {
            vec!["psql", "-q", "-U", &db.user, "-d", "postgres", "-f", "-"]
        } else {
            vec![
                "sh",
                "-c",
                "if command -v mariadb >/dev/null 2>&1; then exec mariadb -uroot; else exec mysql -uroot; fi",
            ]
        };
        let loaded = self.host.open(Path::new(&dump)).map_err(|e| e.to_string()).and_then(|file| {
            docker
                .clone()
                .with_timeout(Duration::from_secs(60))
                .exec_from(container, &client, &options, &mut GzDecoder::new(file))
                .map_err(|e| e.to_string())
        });
        match loaded {
            // a cluster dump recreates roles that exist: psql reports those and goes on
            Ok(_) => self.ok(&format!("{}: dump loaded ({dump})", db.name)),
            Err(e) => self.warn(&format!("{}: could not load {dump}: {e}", db.name)),
        }
    }
}

/// Host directories that hold a database's raw data. Nothing may be put into them before the
/// database has started for the first time: it initializes only a directory that is empty.
pub(super) fn database_dirs(run: &Run<'_>) -> Vec<String> {
    parsed(run, "docker/containers.json")
        .iter()
        .chain(&parsed(run, "docker/services.json"))
        .filter_map(database)
        .flat_map(|db| db.data)
        .filter(|(kind, _)| kind != "volume")
        .map(|(_, source)| source)
        .collect()
}

pub(super) fn docker(run: &mut Run<'_>) {
    let docker = Docker::new(Arc::clone(run.host));
    if !run.dry() && !docker.ping() {
        run.warn("Docker does not answer: run the docker-engine phase first (`moi restore --from docker-engine`)");
        return;
    }
    let containers = parsed(run, "docker/containers.json");
    let existing_networks: Vec<String> =
        docker.networks().unwrap_or_default().iter().map(|n| text(&n["Name"]).to_string()).collect();
    let network_spec = |n: &Value| {
        let subnets: Vec<Value> = n["subnets"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| json!({ "Subnet": s["subnet"], "Gateway": s["gateway"] }))
            .collect();
        json!({
            "Name": n["name"], "Driver": n["driver"], "Options": n["options"].as_object().cloned().unwrap_or_default(),
            // compose finds its own networks by these labels and adopts them instead of failing on the name
            "Labels": n["labels"].as_object().cloned().unwrap_or_default(),
            "Attachable": n["attachable"].as_bool().unwrap_or(false), "IPAM": { "Config": subnets },
        })
    };
    let networks = parsed(run, "docker/networks.json");
    let builtin = ["bridge", "host", "none", "ingress", "docker_gwbridge"];
    // ---- networks with the subnets they had (addresses in configs and firewall rules stay true)
    for network in networks.iter().filter(|n| text(&n["scope"]) == "local" && !builtin.contains(&text(&n["name"]))) {
        let name = text(&network["name"]);
        if existing_networks.iter().any(|e| e == name) {
            continue;
        }
        if ["macvlan", "ipvlan"].contains(&text(&network["driver"])) {
            run.warn(&format!(
                "network {name} is {}: check that its parent interface exists under that name here",
                text(&network["driver"])
            ));
        }
        if run.dry() {
            let subnets: Vec<&str> =
                network["subnets"].as_array().into_iter().flatten().map(|s| text(&s["subnet"])).collect();
            run.say(&format!(
                "    [dry-run] create network {name} ({} {})",
                text(&network["driver"]),
                subnets.join(" ")
            ));
        } else if let Err(e) = docker.network_create(&network_spec(network)) {
            run.warn(&format!("could not create the network {name}: {e}"));
        }
    }

    // ---- compose stacks: the configured boot order first, then the rest by name
    let mut projects = parsed(run, "docker/compose-projects.json");
    let order: Vec<String> = run.old.as_ref().map(|l| l.config.boot.compose_up.clone()).unwrap_or_default();
    let rank = |project: &Value| {
        let dir = text(&project["workdir"]);
        order
            .iter()
            .position(|entry| {
                entry == dir || entry.strip_suffix("/**").is_some_and(|top| crate::backup::under(dir, top))
            })
            .unwrap_or(order.len())
    };
    projects.sort_by_key(|p| (rank(p), text(&p["project"]).to_string()));
    let was_running: Vec<String> = run
        .volatile_file("docker-ps.tsv")
        .lines()
        .filter_map(|l| {
            let mut cells = l.split('\t');
            let (name, state) = (cells.next()?, cells.next()?);
            (state == "running").then(|| name.to_string())
        })
        .collect();
    for project in &projects {
        let (name, workdir) = (text(&project["project"]).to_string(), text(&project["workdir"]).to_string());
        let list = |key: &str| -> Vec<String> {
            project[key].as_array().into_iter().flatten().map(|v| text(v).to_string()).collect()
        };
        // the stack's files: what the data phase did not bring back comes from the bundle
        if !workdir.is_empty() {
            run.restore_path(&workdir, true);
        }
        for file in list("config_files")
            .iter()
            .chain(&list("env_files"))
            .filter(|f| workdir.is_empty() || !crate::backup::under(f, &workdir))
        {
            run.restore_path(file, true);
        }
        let members = list("containers");
        if !members.iter().any(|c| was_running.contains(c)) {
            run.info(&format!("stack {name} was not running on the old server: its files are back, it is not started"));
            continue;
        }
        let mut base: Vec<String> = vec!["compose".into(), "-p".into(), name.clone()];
        if !workdir.is_empty() {
            base.extend(["--project-directory".to_string(), workdir.clone()]);
        }
        for file in list("config_files") {
            base.extend(["-f".to_string(), file]);
        }
        for file in list("env_files") {
            base.extend(["--env-file".to_string(), file]);
        }
        let compose = |run: &mut Run<'_>, rest: &[&str]| {
            let mut command: Vec<&str> = vec!["docker"];
            command.extend(base.iter().map(String::as_str));
            command.extend(rest.iter().copied());
            run.act(&command)
        };
        run.say(&format!("  stack {name}"));
        compose(run, &["pull", "-q", "--ignore-pull-failures"]);
        // databases first: up, ready, and — if their data directory is empty — loaded from their dump
        let databases: Vec<Database> = containers
            .iter()
            .filter(|c| text(&c["labels"]["com.docker.compose.project"]) == name)
            .filter_map(database)
            .filter(|d| !d.service.is_empty())
            .collect();
        if !databases.is_empty() {
            let empty: Vec<bool> = databases.iter().map(|db| run.data_is_empty(&docker, db)).collect();
            let mut up = vec!["up", "-d"];
            up.extend(databases.iter().map(|d| d.service.as_str()));
            if !compose(run, &up) {
                run.warn(&format!("the databases of stack {name} did not start: docker {} logs", base.join(" ")));
            }
            for (db, empty) in databases.iter().zip(empty) {
                if !run.dry() && !run.wait_for(&docker, &db.name, 120) {
                    run.warn(&format!("{} did not become healthy within 2 minutes", db.name));
                }
                if empty {
                    run.import(&docker, db, &db.name.clone());
                } else {
                    run.info(&format!(
                        "{}: its data directory has data (the disk was moved over?): no dump is loaded",
                        db.name
                    ));
                }
            }
        }
        if !compose(run, &["up", "-d"]) {
            run.warn(&format!("stack {name} did not start: docker {} logs", base.join(" ")));
        }
    }

    // ---- swarm: a one-node swarm, its networks, and the services that can be recreated from what was captured
    let services = parsed(run, "docker/services.json");
    if run.snapshot_file("docker/swarm-state").trim() == "active" {
        if run.dry() {
            run.say("    [dry-run] docker swarm init");
        } else if !docker.swarm_active().unwrap_or(false)
            && let Err(e) = docker.swarm_init(None)
        {
            run.warn(&format!("could not start a swarm ({e}): run `docker swarm init --advertise-addr <this machine's address>`, then `moi restore --only docker`"));
        }
        for network in networks.iter().filter(|n| text(&n["scope"]) == "swarm" && !builtin.contains(&text(&n["name"])))
        {
            let name = text(&network["name"]);
            if existing_networks.iter().any(|e| e == name) {
                continue;
            }
            if run.dry() {
                run.say(&format!("    [dry-run] create overlay network {name}"));
            } else if let Err(e) = docker.network_create(&network_spec(network)) {
                run.warn(&format!("could not create the network {name}: {e}"));
            }
        }
        let have: Vec<String> =
            docker.services().unwrap_or_default().iter().map(|s| text(&s["Spec"]["Name"]).to_string()).collect();
        let mut rebuild: Vec<String> = Vec::new();
        for service in &services {
            let name = text(&service["name"]).to_string();
            let image = text(&service["image"]);
            if have.contains(&name) {
                continue;
            }
            // Docker never gives the value of a secret back, so neither the snapshot nor the backup has it
            let sealed: Vec<&str> = ["secrets", "configs"]
                .iter()
                .flat_map(|key| service[*key].as_array().into_iter().flatten().filter_map(Value::as_str))
                .collect();
            if !sealed.is_empty() {
                run.warn(&format!(
                    "service {name} uses Docker secrets or configs ({}) whose values Docker does not give back: create them, then the service (its description is in the bundle, snapshot/docker/services.json)",
                    sealed.join(", ")
                ));
                continue;
            }
            // an image built on the old server for this very service exists nowhere else
            let repository = image.split(':').next().unwrap_or(image);
            if repository == name {
                rebuild.push(name);
                continue;
            }
            let mounts: Vec<Value> = service["mounts"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| json!({ "Type": m["type"], "Source": m["source"], "Target": m["destination"], "ReadOnly": m["ro"].as_bool().unwrap_or(false) }))
                .collect();
            let attach: Vec<Value> = service["networks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|n| json!({ "Target": n["name"], "Aliases": n["aliases"] }))
                .collect();
            let ports: Vec<Value> = service["ports"].as_array().into_iter().flatten().map(|p| json!({ "Protocol": p["protocol"], "TargetPort": p["target"], "PublishedPort": p["published"], "PublishMode": p["mode"] })).collect();
            let mut container = json!({ "Image": image, "Env": service["env"].as_array().cloned().unwrap_or_default(), "Mounts": mounts });
            for (key, field) in
                [("command", "Command"), ("args", "Args"), ("hostname", "Hostname"), ("container_labels", "Labels")]
            {
                if !service[key].is_null() {
                    container[field] = service[key].clone();
                }
            }
            let spec = json!({
                "Name": name, "Labels": service["labels"].as_object().cloned().unwrap_or_default(),
                "TaskTemplate": {
                    "ContainerSpec": container,
                    "RestartPolicy": { "Condition": service["restart"].as_str().unwrap_or("any") },
                    "Placement": { "Constraints": service["constraints"].as_array().cloned().unwrap_or_default() },
                    "Networks": attach,
                },
                "Mode": { "Replicated": { "Replicas": service["replicas"].as_u64().unwrap_or(1) } },
                "EndpointSpec": { "Ports": ports },
            });
            let db = database(service);
            let empty = db.as_ref().is_some_and(|db| run.data_is_empty(&docker, db));
            if run.dry() {
                run.say(&format!("    [dry-run] create service {name} ({image})"));
            } else if let Err(e) = docker.service_create(&spec) {
                run.warn(&format!("could not create the service {name}: {e}"));
                continue;
            }
            if let Some(db) = db.filter(|_| empty) {
                // the service's task is the container to load the dump into
                let task = || {
                    docker
                        .containers(false)
                        .unwrap_or_default()
                        .iter()
                        .find(|c| c["Labels"]["com.docker.swarm.service.name"] == name.as_str())
                        .and_then(|c| c["Id"].as_str().map(str::to_string))
                };
                let mut waited = 0;
                while !run.dry() && task().is_none() && waited < 120 {
                    run.host.sleep(Duration::from_secs(2));
                    waited += 2;
                }
                match task() {
                    Some(container) => run.import(&docker, &db, &container),
                    None if run.dry() => run.import(&docker, &db, &name),
                    None => {
                        run.warn(&format!("{name}: its task did not start within 2 minutes; its dump was not loaded"))
                    }
                }
            }
        }
        if !rebuild.is_empty() {
            run.warn(&format!("these services ran images that were built on the old server and exist nowhere else — deploy them again from where they were built (Dokploy: open each application and redeploy): {}", rebuild.join(" ")));
        }
    }

    // ---- containers nobody wrote a compose file for: their generated scripts, reviewed one by one
    let standalone = run.snap.join("docker/standalone");
    let running_now: Vec<String> = docker
        .containers(true)
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
    for file in run.host.read_dir(&standalone).unwrap_or_default() {
        let Some(name) = file.strip_suffix(".sh") else { continue };
        if running_now.iter().any(|n| n == name) {
            continue;
        }
        let script = run.host.read_to_string(&standalone.join(&file)).unwrap_or_default();
        run.say("");
        for line in script.lines() {
            // values of the environment stay in the file: the terminal and the log get the names
            match line.trim_start().strip_prefix("-e ").and_then(|e| e.split_once('=')) {
                Some((key, _)) => run.say(&format!("      -e {key}=… \\")),
                None => run.say(&format!("    {line}")),
            }
        }
        if run.ask(&format!("Recreate the standalone container '{name}' with the command above?"), true) {
            let script = standalone.join(&file).to_string_lossy().into_owned();
            if !run.act(&["sh", &script]) {
                run.warn(&format!("container {name} could not be created"));
            }
        }
    }
}

/// devbox (15): the VM is rebuilt from the restored `/etc/devbox` — its disk was never in the backup
/// — or adopted when the disk came along. No question is asked: the configuration says what the VM
/// is, and the restored secrets hold the password hashes.
pub(super) fn devbox(run: &mut Run<'_>) {
    if !run.host.exists(Path::new(crate::devbox::CONFIG)) {
        run.info(
            "no /etc/devbox on this machine: the old server had no devbox, or its configuration was not in the backup",
        );
        return;
    }
    let Some(loaded) = restored_config(run) else {
        run.warn("moi's configuration is not in place (run the moi phase first): devbox was not rebuilt — `moi devbox up` afterwards");
        return;
    };
    let instance = run
        .host
        .read_to_string(Path::new(crate::devbox::CONFIG))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|config| config.get("instance").and_then(toml::Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "devbox".to_string());
    // there already (its disk was moved over): only the configuration is reconciled
    let exists = run
        .look(&["incus", "list", "-c", "n", "-f", "csv"])
        .is_some_and(|o| o.status == 0 && o.stdout_str().lines().any(|l| l.trim() == instance));
    run.info(&if exists {
        format!("the instance {instance} exists: adopted, its disk and data kept")
    } else {
        format!("the instance {instance} does not exist: rebuilt from /etc/devbox (its disk was never backed up)")
    });
    let state = StateDir::new(loaded.config.state.dir.clone());
    match crate::devbox::rebuild(run.host, &loaded.config, &state, exists, run.dry()) {
        Ok((text, health)) => {
            for line in text.lines() {
                run.say(&format!("    {line}"));
            }
            match health.as_deref() {
                Some("ok") => {
                    run.ok("devbox answers: health ok. Repositories come back with its own sync on the first wake")
                }
                Some(other) => {
                    run.warn(&format!("devbox is back but reports its health as {other}: `moi devbox doctor`"))
                }
                None if run.dry() => {}
                None => run
                    .warn("devbox was set up but its health could not be asked: `moi devbox status` after the restore"),
            }
        }
        Err(why) => run.warn(&format!("devbox was not rebuilt: {why}")),
    }
}

/// SSH from anywhere (16): the connector as a host service again, from the old server's `[ssh]` and
/// secrets. The ingress rule and the DNS record are Cloudflare's to remember: they are confirmed, and
/// only changed if they are not what they were.
pub(super) fn ssh(run: &mut Run<'_>) {
    let Some(old) = run.old.clone() else {
        run.info("the bundle's moi.toml does not load: `moi ssh up` by hand once moi is configured");
        return;
    };
    let Some(settings) = crate::ssh::settings_of(run.host.as_ref(), &old.config) else {
        run.info("the old server had no [ssh]");
        return;
    };
    let provider = crate::ssh::provider(settings.provider);
    let plan = provider.plan(run.host, settings);
    for line in crate::ssh::render(&plan).lines().filter(|l| l.starts_with("  ")) {
        run.say(&format!("  {line}"));
    }
    if let Some(stop) = plan.stopped() {
        run.warn(&format!("SSH from anywhere was not set up ({}): `moi ssh up` when that is solved", stop.name));
        return;
    }
    if run.dry() {
        run.say("    [dry-run] moi ssh up --yes; then the SSH handshake through the public name");
        return;
    }
    let state = StateDir::new(old.config.state.dir.clone());
    let done = state
        .ensure(run.host.as_ref())
        .and_then(|()| state.open_db(run.host.as_ref()))
        .map_err(|e| e.to_string())
        .and_then(|db| {
            let cx = crate::ssh::apply::Cx { host: run.host, loaded: &old, state: &state, db: &db };
            let archive = crate::ssh::apply::new_archive(run.host.as_ref(), &state);
            provider.apply(&cx, &plan, &archive)
        });
    match done {
        Ok(lines) => {
            for line in lines {
                run.info(&line);
            }
        }
        Err(why) => {
            run.warn(&format!("SSH from anywhere: {why}"));
            return;
        }
    }
    if plan.tunnel.is_some() {
        match crate::ssh::apply::handshake(run.host, &plan.settings.hostname) {
            Ok(ms) => run.ok(&format!(
                "an SSH server answers at https://{}/ ({ms} ms): `ssh {}` works again",
                plan.settings.hostname, plan.settings.alias
            )),
            Err(e) => run.warn(&format!(
                "the public SSH handshake failed ({}: {}): `moi ssh status` after the restore",
                e.stage, e.error
            )),
        }
    }
}

pub(super) fn moi(run: &mut Run<'_>) {
    // the configuration and the secrets as they were (a configuration already here is kept next to them)
    let etc = run.etc_moi.clone();
    let mut stack = vec![etc.clone()];
    let mut files = Vec::new();
    while let Some(at) = stack.pop() {
        match run.host.symlink_metadata(&at).map(|m| m.kind) {
            Ok(crate::host::FileKind::Dir) => {
                stack.extend(run.host.read_dir(&at).unwrap_or_default().into_iter().map(|n| at.join(n)))
            }
            Ok(_) => files.push(at),
            Err(_) => {}
        }
    }
    files.sort();
    if run.dry() {
        run.say(&format!("    [dry-run] restore /etc/moi ({} files)", files.len()));
    }
    for from in &files {
        let Ok(relative) = from.strip_prefix(&etc) else { continue };
        let target = Path::new("/etc/moi").join(relative);
        if run.dry() {
            continue;
        }
        let Ok(content) = run.host.read(from) else { continue };
        if let Some(parent) = target.parent() {
            let _ = run.host.create_dir_all(parent, 0o755);
        }
        if let Ok(existing) = run.host.read(&target) {
            if existing == content {
                continue;
            }
            let _ = run.host.write(Path::new(&format!("{}.pre-moi", target.display())), &existing, 0o600);
        }
        let mode = if target.ends_with("secrets.toml") { 0o600 } else { 0o644 };
        if let Err(e) = run.host.write(&target, &content, mode) {
            run.warn(&format!("could not write {}: {e}", target.display()));
        }
    }
    // the history goes on where the old server's stopped
    let state_dir = run.old.as_ref().map_or_else(|| "/var/lib/moi".to_string(), |l| l.config.state.dir.clone());
    let state = StateDir::new(state_dir.clone());
    let bundle_state = run.snap.parent().map(Path::to_path_buf).unwrap_or_default();
    if run.host.exists(&state.state().join(".git")) {
        run.info(&format!("{} already has a history: kept", state.state().display()));
    } else if run.dry() {
        run.say(&format!("    [dry-run] copy the old server's state and history to {}", state.state().display()));
    } else {
        let mut copied = 0;
        let mut stack = vec![bundle_state.clone()];
        while let Some(at) = stack.pop() {
            let Ok(relative) = at.strip_prefix(&bundle_state) else { continue };
            let target = state.state().join(relative);
            match run.host.symlink_metadata(&at).map(|m| m.kind) {
                Ok(crate::host::FileKind::Dir) => {
                    let _ = run.host.create_dir_all(&target, 0o700);
                    stack.extend(run.host.read_dir(&at).unwrap_or_default().into_iter().map(|n| at.join(n)));
                }
                Ok(_) => {
                    if let Ok(content) = run.host.read(&at) {
                        let mode = run.host.metadata(&at).map_or(0o600, |m| m.mode);
                        if run.host.write(&target, &content, mode).is_ok() {
                            copied += 1;
                        }
                    }
                }
                Err(_) => {}
            }
        }
        run.ok(&format!("state and history copied to {} ({copied} files)", state.state().display()));
    }
    if run.dry() {
        run.say("    [dry-run] install /etc/systemd/system/moi.service; systemctl daemon-reload; systemctl enable --now moi.service");
    } else {
        match crate::init::install_service(run.host.as_ref()) {
            Ok(binary) => run.ok(&format!("moi.service installed and started ({binary} daemon)")),
            Err(e) => run.warn(&format!("the service was not installed: {e}")),
        }
    }
}

/// The restored configuration of this machine, once the moi phase has put it in place.
fn restored_config(run: &Run<'_>) -> Option<crate::config::Loaded> {
    // a dry run has put nothing in place: it reads what would be
    if run.dry() {
        return run.old.clone();
    }
    crate::config::load(run.host.as_ref(), &crate::config::Paths::new("/etc/moi/moi.toml".into(), None)).ok()
}

pub(super) fn firewall(run: &mut Run<'_>) {
    let old: Vec<String> = run
        .snapshot_lines("network/links.txt")
        .iter()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .filter(|i| i != "lo")
        .collect();
    run.info(&format!("rules may name the old machine's interfaces: {}", old.join(" ")));
    let Some(loaded) = restored_config(run) else {
        run.warn("moi's configuration is not in place (run the moi phase first): the firewall was not touched");
        return;
    };
    if !loaded.config.firewall.manage {
        run.info("the old server's firewall was not managed by moi: its rule files came back with /etc; enable its own tool (ufw, nftables, iptables-persistent) by hand after checking the interface names");
        return;
    }
    run.info(&format!("managed chains: {}", loaded.config.firewall.chains.join(", ")));
    if run.dry() {
        run.say("    [dry-run] moi firewall apply (previous rules saved; over SSH the rollback is scheduled first and the rules kept only when confirmed)");
        return;
    }
    // never taken by --yes: a wrong rule on a machine reached over the network locks its owner out
    if run.prompt("Type  yes  to apply the firewall now (the rules in place are saved first; over SSH they come back unless you confirm):") != "yes" {
        run.info("not applied: `moi firewall apply` when you are ready");
        return;
    }
    let state = StateDir::new(loaded.config.state.dir.clone());
    let applied = state
        .open_db(run.host.as_ref())
        .and_then(|db| crate::firewall::apply_guarded(run.host, &loaded.config, &state, &db));
    match applied {
        Ok(done) => run.ok(&format!(
            "firewall applied: {}",
            done.applied
                .iter()
                .map(|a| format!("{} rules ({})", a.rules, a.family.name()))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Err(e) => run.warn(&format!("the firewall was not applied: {e}")),
    }
}

pub(super) fn monitors(run: &mut Run<'_>) {
    let Some(loaded) = restored_config(run) else {
        run.warn("moi's configuration is not in place (run the moi phase first)");
        return;
    };
    if !loaded.config.monitors.kuma.enabled {
        run.info("no Uptime Kuma is configured");
        return;
    }
    if run.dry() {
        run.say("    [dry-run] moi monitors sync --dry-run: every monitor must be `keep` or `adopt` (Kuma's data came back with the backup; push tokens are unchanged)");
        return;
    }
    let docker = Docker::new(Arc::clone(run.host));
    let state = StateDir::new(loaded.config.state.dir.clone());
    let plan = state.open_db(run.host.as_ref()).and_then(|db| {
        let inventory = crate::monitors::local_inventory(run.host.as_ref(), &loaded.config, &docker)?;
        crate::monitors::plan_for(
            run.host.as_ref(),
            &loaded.config,
            &docker,
            &state,
            &db,
            &inventory,
            crate::monitors::Options::default(),
        )
    });
    match plan {
        Ok(plan) => {
            let create: Vec<&str> = plan
                .steps
                .iter()
                .filter(|s| s.action == crate::monitors::Action::Create)
                .map(|s| s.monitor.as_str())
                .collect();
            if create.is_empty() {
                run.ok("every monitor moi uses is there, with the token it had: pushes go on as before");
            } else {
                run.warn(&format!(
                    "these monitors would have to be created — Kuma's data did not all come back: {}",
                    create.join(", ")
                ));
            }
        }
        Err(e) => run.warn(&format!(
            "could not look at Uptime Kuma: {e} (is its container up? `moi monitors sync --dry-run` later)"
        )),
    }
}

pub(super) fn verify(run: &mut Run<'_>) {
    if run.dry() {
        run.say(
            "    [dry-run] moi verify against what ran on the old server; then the reboot that proves it survives one",
        );
        return;
    }
    let Some(loaded) = restored_config(run) else {
        run.warn("moi's configuration is not in place (run the moi phase first): nothing to verify against");
        return;
    };
    let state = StateDir::new(loaded.config.state.dir.clone());
    let config = Arc::new(loaded.config);
    let Ok(db) = state.open_db(run.host.as_ref()) else { return };
    // held to what ran on the old server, not to what happens to run here now
    let bundle = StateDir::new(run.snap.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default());
    let _ = super::verify::Expect::from_state(run.host.as_ref(), &bundle).save(run.host.as_ref(), &state);
    match super::verify::verify(run.host, &config, &state, &db) {
        Ok(items) => {
            for line in super::verify::render(&items).lines() {
                run.say(&format!("  {line}"));
            }
            if items.iter().any(|i| !i.ok) {
                run.warnings += 1;
            }
        }
        Err(e) => run.warn(&format!("verification could not run: {e}")),
    }
    // a rebuild is only done when it has survived a reboot: the daemon verifies again after it
    if let Some(boot) =
        run.host.read_to_string(Path::new("/proc/sys/kernel/random/boot_id")).ok().map(|b| b.trim().to_string())
    {
        super::verify::expect_verification(&db, &boot);
    }
    run.say("");
    run.say("  Last step — reboot, so that what was rebuilt is proven to come up on its own:");
    run.say("      sudo systemctl reboot");
    run.say(&format!(
        "  {} after the boot the daemon runs this verification again; its result is in the login message",
        config.boot.verify_after
    ));
    run.say("  (/run/motd.d/50-moi), in `journalctl -t moi MOI_JOB=verify`, and on the boot monitor if you have one.");
}
