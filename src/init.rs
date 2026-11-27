//! `moi init` and `moi uninstall`: discover what the server has and write a first configuration from
//! it; install the one unit moi has; and take both away again (docs/spec/01-config.md § Discovery).
//!
//! Discovery proposes, it never invents: disks, mounts and paths come from what is there; checks that
//! need judgement (which URL means an app is healthy) are written as commented examples.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use crate::docker::Docker;
use crate::error::Error;
use crate::host::{self, Cmd, FileKind, Host};

pub const UNIT_PATH: &str = "/etc/systemd/system/moi.service";
const BINARIES: [&str; 2] = ["/usr/bin/moi", "/usr/local/bin/moi"];
/// Names that mean a directory holds replaceable bulk data.
const FUNGIBLE: [&str; 9] =
    ["media", "movies", "tv", "music", "downloads", "download", "torrents", "usenet", "transcodes"];
/// Bind-mount sources that are the system's, not the server's identity.
const SYSTEM_SOURCES: [&str; 9] =
    ["/proc", "/sys", "/dev", "/run", "/var/run", "/var/lib/docker", "/etc", "/lib", "/usr"];

/// What discovery found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Found {
    pub server: String,
    pub data_roots: Vec<String>,
    pub fungible: Vec<String>,
    /// (device, smartctl type)
    pub disks: Vec<(String, String)>,
    pub mounts: Vec<String>,
    pub writable: Vec<String>,
    pub docker: bool,
    /// Commented example lines for `checks.services.http` / `.exec`.
    pub http_examples: Vec<String>,
    pub exec_examples: Vec<String>,
    pub kuma_url: Option<String>,
    pub backup_paths: Vec<String>,
    /// A restic environment file an existing backup uses.
    pub restic_env: Option<String>,
    /// Signs of an existing home-grown setup.
    pub legacy: Vec<String>,
}

fn is_dir(host: &dyn Host, path: &str) -> bool {
    host.metadata(Path::new(path)).is_ok_and(|m| m.kind == FileKind::Dir)
}

fn fungible_looking(path: &str) -> bool {
    path.split('/').any(|part| FUNGIBLE.contains(&part.to_lowercase().as_str()))
}

/// The whole-disk device a partition or volume is on, if it is directly on one (`/dev/sda1` → `sda`).
fn disk_of(source: &str, disks: &[String]) -> Option<String> {
    let name = source.strip_prefix("/dev/")?;
    disks.iter().filter(|disk| name.starts_with(disk.as_str())).max_by_key(|disk| disk.len()).cloned()
}

/// Look at the machine.
pub fn discover(host: &Arc<dyn Host>) -> Found {
    let h = host.as_ref();
    let mut found = Found { server: host::short_hostname(h), ..Found::default() };

    // ---- storage: physical disks, and which of them hang on USB
    let block = Path::new("/sys/block");
    let names: Vec<String> = h
        .read_dir(block)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| !["loop", "ram", "zram", "dm-", "sr", "md", "fd", "nbd"].iter().any(|p| n.starts_with(p)))
        .collect();
    let on_usb = |name: &str| {
        h.read_link(&block.join(name)).is_ok_and(|target| target.to_string_lossy().contains("/usb"))
            || h.read_to_string(&block.join(name).join("removable")).is_ok_and(|r| r.trim() == "1")
    };
    for name in &names {
        // USB bridges need to be told to pass SMART commands through
        let kind = if name.starts_with("nvme") {
            "nvme"
        } else if on_usb(name) {
            "sat"
        } else {
            "auto"
        };
        found.disks.push((format!("/dev/{name}"), kind.to_string()));
    }
    // ---- real filesystems (not the kernel's, not container layers)
    let real = [
        "ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "f2fs", "ntfs", "ntfs3", "exfat", "vfat", "bcachefs", "nfs",
        "nfs4", "cifs",
    ];
    let mounted: Vec<_> =
        crate::snapshot::mounts(h).into_iter().filter(|m| real.contains(&m.fstype.as_str())).collect();
    let system = |target: &str| {
        target == "/"
            || target.starts_with("/boot")
            || target == "/var"
            || target.starts_with("/var/lib/")
            || target.starts_with("/snap")
    };
    found.mounts.push("/".to_string());
    for mount in mounted.iter().filter(|m| !system(&m.target)) {
        found.data_roots.push(mount.target.clone());
        found.mounts.push(mount.target.clone());
        if disk_of(&mount.source, &names).is_some_and(|disk| on_usb(&disk)) {
            found.writable.push(mount.target.clone());
        }
    }
    for dir in ["/opt", "/srv", "/home"] {
        if !h.read_dir(Path::new(dir)).unwrap_or_default().is_empty() {
            found.data_roots.push(dir.to_string());
        }
    }
    found.data_roots.sort();
    found.data_roots.dedup();

    // ---- containers: examples for checks, Kuma, what to back up
    let docker = Docker::new(Arc::clone(host));
    found.docker = docker.ping();
    let mut stacks: BTreeSet<String> = BTreeSet::new();
    let mut sources: BTreeSet<String> = BTreeSet::new();
    for container in docker.containers_inspect().unwrap_or_default() {
        let labels = &container["Config"]["Labels"];
        if labels["com.docker.swarm.task.id"].as_str().is_some_and(|id| !id.is_empty()) {
            continue;
        }
        let name = container["Name"].as_str().unwrap_or_default().trim_start_matches('/').to_string();
        let image = container["Config"]["Image"].as_str().unwrap_or_default();
        let base = image
            .split('@')
            .next()
            .unwrap_or_default()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .split(':')
            .next()
            .unwrap_or_default()
            .to_string();
        if let Some(dir) = labels["com.docker.compose.project.working_dir"].as_str() {
            stacks.insert(dir.to_string());
        }
        for mount in container["Mounts"].as_array().into_iter().flatten() {
            if mount["Type"] == "bind"
                && let Some(source) = mount["Source"].as_str()
            {
                sources.insert(source.to_string());
            }
        }
        // a database answers its own client; anything with a port may answer HTTP
        match (crate::backup::database_engine(image, &container["Config"]["Env"]), base.as_str()) {
            (Some("postgres"), _) => found
                .exec_examples
                .push(format!("{{ name = \"{name}\", container = \"{name}\", cmd = \"pg_isready -q\" }}")),
            (Some(_), _) => found
                .exec_examples
                .push(format!("{{ name = \"{name}\", container = \"{name}\", cmd = \"mysqladmin ping --silent\" }}")),
            (None, "redis" | "valkey" | "keydb") => found.exec_examples.push(format!(
                "{{ name = \"{name}\", container = \"{name}\", cmd = \"redis-cli ping\", expect = \"PONG\" }}"
            )),
            _ => {
                let mut ports: Vec<u32> = container["Config"]["ExposedPorts"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(p, _)| p.ends_with("/tcp"))
                    .filter_map(|(p, _)| p.split('/').next()?.parse().ok())
                    .collect();
                ports.sort_unstable();
                let networks = container["NetworkSettings"]["Networks"].as_object();
                let reachable = networks
                    .is_some_and(|n| n.values().any(|net| net["IPAddress"].as_str().is_some_and(|ip| !ip.is_empty())));
                if let (Some(port), true) = (ports.first(), reachable) {
                    found
                        .http_examples
                        .push(format!("{{ name = \"{name}\", target = \"{name}\", path = \"{port}/\" }}"));
                }
            }
        }
        if base == "uptime-kuma" && found.kuma_url.is_none() {
            // published on the host, or else reachable at the container's own address
            let published =
                container["NetworkSettings"]["Ports"]["3001/tcp"][0]["HostPort"].as_str().filter(|p| !p.is_empty());
            let address = container["NetworkSettings"]["Networks"]
                .as_object()
                .and_then(|n| n.values().find_map(|net| net["IPAddress"].as_str().filter(|ip| !ip.is_empty())));
            found.kuma_url = published
                .map(|port| format!("http://127.0.0.1:{port}"))
                .or_else(|| address.map(|ip| format!("http://{ip}:3001")));
        }
    }
    // ---- what to back up: stack directories, and data that containers keep outside them
    let stacks: Vec<String> = stacks.into_iter().filter(|dir| is_dir(h, dir)).collect();
    // a directory of stacks is one path, not many
    let mut parents: BTreeSet<String> = BTreeSet::new();
    for stack in &stacks {
        let parent = Path::new(stack).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let siblings = stacks.iter().filter(|s| s.starts_with(&format!("{parent}/"))).count();
        parents.insert(if siblings > 1 && parent.matches('/').count() >= 2 { parent } else { stack.clone() });
    }
    let mut paths: Vec<String> = parents.into_iter().collect();
    for source in sources {
        let system = SYSTEM_SOURCES.iter().any(|s| crate::backup::under(&source, s)) || source == "/";
        if system || !is_dir(h, &source) {
            continue;
        }
        if fungible_looking(&source) {
            found.fungible.push(source);
        } else if !paths.iter().any(|p| crate::backup::under(&source, p)) {
            paths.push(source);
        }
    }
    // the outermost of nested paths is enough
    let all = paths.clone();
    paths.retain(|p| !all.iter().any(|other| other != p && crate::backup::under(p, other)));
    let fungible = found.fungible.clone();
    found.fungible.retain(|p| !fungible.iter().any(|other| other != p && crate::backup::under(p, other)));
    paths.sort();
    found.backup_paths = paths;

    // ---- what is there already
    found.restic_env =
        ["/etc/restic/env", "/etc/restic.env", "/root/.restic-env", "/etc/default/restic", "/opt/scripts/restic.env"]
            .iter()
            .find(|f| h.exists(Path::new(f)))
            .map(ToString::to_string);
    for (path, what) in [
        ("/var/lib/serverstate", "serverstate state directory"),
        ("/etc/serverstate.conf", "serverstate configuration"),
    ] {
        if h.exists(Path::new(path)) {
            found.legacy.push(format!("{what} ({path})"));
        }
    }
    for dir in ["/etc/cron.d", "/opt/scripts", "/usr/local/bin"] {
        for name in h.read_dir(Path::new(dir)).unwrap_or_default() {
            let file = Path::new(dir).join(&name);
            if h.metadata(&file).is_ok_and(|m| m.kind == FileKind::File && m.size < 200_000)
                && h.read_to_string(&file).is_ok_and(|text| text.contains("/api/push/"))
            {
                found.legacy.push(format!("a script that pushes to Uptime Kuma ({})", file.display()));
            }
        }
    }
    found
}

fn list(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|i| format!("{i:?}")).collect::<Vec<_>>().join(", "))
}

fn commented_examples(key: &str, examples: &[String]) -> String {
    let mut out = format!("{key} = [\n");
    for example in examples {
        out.push_str(&format!("  # {example},\n"));
    }
    out.push_str("]\n");
    out
}

/// The configuration for what was found. `observe`: only what describing the server needs.
pub fn config_text(found: &Found, observe: bool) -> String {
    let mut out = String::from(
        "# moi.toml — written by `moi init` from what it found on this server.\n# Every section is optional: delete what you do not want, `moi config check` validates the rest.\n# All keys with their defaults: examples/moi.toml in the moi repository.\n\n",
    );
    out.push_str(&format!("[server]\nname = {:?}\n\n", found.server));
    if !found.data_roots.is_empty() {
        out.push_str(&format!("[capture]\ndata_roots = {}\n\n", list(&found.data_roots)));
    }
    if !found.fungible.is_empty() {
        out.push_str("# These look like media or downloads. Declared replaceable, they are never backed up and never\n# reported as unprotected. Remove any that you could not get back.\n");
        out.push_str(&format!("[data]\nfungible = {}\n\n", list(&found.fungible)));
    }
    if observe {
        out.push_str("# Written with --observe: moi only describes this server (snapshot, context, audit).\n# Run `moi init` again after deleting this file to add checks, monitors and backups.\n");
        return out;
    }
    out.push_str("# ------------------------------------------------------------------ checks\n[checks.resources]\n\n[checks.storage]\n");
    out.push_str(&format!("mounts = {}\n", list(&found.mounts)));
    if !found.writable.is_empty() {
        out.push_str(&format!(
            "writable = {}        # removable disks: must be mounted and writable\n",
            list(&found.writable)
        ));
    }
    if !found.disks.is_empty() {
        out.push_str("disks = [\n");
        for (device, kind) in &found.disks {
            out.push_str(&format!("  {{ device = {device:?}, type = {kind:?} }},\n"));
        }
        out.push_str("]\n");
    }
    if found.docker {
        out.push_str("\n# Containers and swarm services are checked as they are. What \"healthy\" means for an app is yours to\n# say: these examples come from what runs here — uncomment and adjust the ones you want.\n[checks.services]\n");
        out.push_str(&commented_examples("http", &found.http_examples));
        out.push_str(&commented_examples("exec", &found.exec_examples));
    }
    if let Some(url) = &found.kuma_url {
        out.push_str("\n# ------------------------------------------------------------------ monitors\n# `moi monitors sync --dry-run` shows what would be adopted or created; nothing changes until you sync.\n[monitors.kuma]\n");
        out.push_str(&format!("url = {url:?}\n# username = \"secret:kuma_username\"     # a login is only needed to create monitors;\n# password = \"secret:kuma_password\"     # put the values into secrets.toml\n"));
    }
    if !found.backup_paths.is_empty() {
        // a proposal, not a decision: where archives go and what is worth keeping is the owner's call
        let local = found
            .writable
            .first()
            .or_else(|| found.data_roots.iter().find(|root| !["/opt", "/srv", "/home"].contains(&root.as_str())));
        let local_dir = local.map_or_else(|| "/var/backups/moi".to_string(), |root| format!("{root}/backups"));
        out.push_str("\n# ------------------------------------------------------------------ backup\n# A proposal from the compose stacks and the directories containers keep their data in. Remove the\n# leading `# ` to switch backups on; `moi audit` shows what is and is not covered either way.\n");
        out.push_str(&format!(
            "# [backup]\n# local_dir = {local_dir:?}        # where the nightly archive is kept\n# paths = [\n"
        ));
        for path in &found.backup_paths {
            out.push_str(&format!("#   {path:?},\n"));
        }
        out.push_str("# ]\n");
        if let Some(env) = &found.restic_env {
            out.push_str(&format!(
                "# An existing restic setup was found ({env}): its repository can be adopted, see [backup.offsite].\n"
            ));
        }
    }
    out
}

/// The unit moi installs: the only one.
pub fn unit_text(binary: &str) -> String {
    format!(
        "# Installed by `moi init`; removed by `moi uninstall`.\n[Unit]\nDescription=moi — server state, health checks, backups, monitors\nAfter=network-online.target docker.service local-fs.target\nWants=network-online.target\n\n[Service]\nType=notify\nExecStart={binary} daemon\nRestart=always\nRestartSec=5\nWatchdogSec=60\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

fn systemctl(host: &dyn Host, args: &[&str]) -> Result<(), Error> {
    let out = host
        .run(&Cmd::new("systemctl").args(args.iter().copied()))
        .map_err(|e| Error::host("cannot run systemctl (moi needs systemd)", e))?;
    if out.status == 0 {
        Ok(())
    } else {
        Err(Error::Usage(format!("systemctl {} failed: {}", args.join(" "), out.stderr_str().trim())))
    }
}

/// Where the binary a service can start is, or what to do about it not being there.
pub fn binary(host: &dyn Host) -> Result<&'static str, Error> {
    BINARIES.iter().copied().find(|b| host.exists(Path::new(b))).ok_or_else(|| {
        Error::Usage(
            "moi is not installed where a service can start it: put the binary at /usr/local/bin/moi (or install the package) and run `moi init` again — or use `moi init --observe` to only describe this server"
                .to_string(),
        )
    })
}

/// Write the unit and start the service.
pub fn install_service(host: &dyn Host) -> Result<String, Error> {
    let binary = binary(host)?;
    host.write(Path::new(UNIT_PATH), unit_text(binary).as_bytes(), 0o644)
        .map_err(|e| Error::host(format!("cannot write {UNIT_PATH}"), e))?;
    systemctl(host, &["daemon-reload"])?;
    systemctl(host, &["enable", "--now", "moi.service"])?;
    Ok(binary.to_string())
}

/// What the existing configuration lacks of what discovery would write (shown, never applied).
pub fn suggestions(found: &Found, existing: &crate::config::Config) -> Vec<String> {
    let mut out = Vec::new();
    let missing = |have: &[String], want: &[String]| -> Vec<String> {
        want.iter().filter(|w| !have.iter().any(|h| crate::backup::under(w, h))).cloned().collect()
    };
    if !existing.checks.resources.common.enabled {
        out.push("[checks.resources] is not enabled".to_string());
    }
    if !existing.checks.storage.common.enabled {
        out.push("[checks.storage] is not enabled".to_string());
    } else if let crate::config::AutoOr::List(disks) = &existing.checks.storage.disks {
        for (device, kind) in found.disks.iter().filter(|(device, _)| !disks.iter().any(|d| d.device == *device)) {
            out.push(format!("checks.storage.disks: {{ device = {device:?}, type = {kind:?} }} is not checked"));
        }
    }
    for mount in found.mounts.iter().filter(|m| !existing.checks.storage.mounts.contains(m)) {
        out.push(format!("checks.storage.mounts: {mount:?} is not checked"));
    }
    if found.docker && !existing.checks.services.common.enabled {
        out.push("[checks.services] is not enabled (containers are running here)".to_string());
    }
    if found.kuma_url.is_some() && !existing.monitors.kuma.enabled {
        out.push(format!(
            "[monitors.kuma] is not set up (Uptime Kuma runs here: url = {:?})",
            found.kuma_url.clone().unwrap_or_default()
        ));
    }
    let covered: Vec<String> = existing.backup.paths.iter().chain(&existing.data.fungible).cloned().collect();
    for path in missing(&covered, &found.backup_paths) {
        out.push(format!("backup.paths: {path:?} is neither backed up nor declared fungible"));
    }
    for path in missing(&covered, &found.fungible) {
        out.push(format!("data.fungible: {path:?} looks replaceable and is not declared"));
    }
    out
}

/// Remove a directory and everything in it.
pub fn remove_tree(host: &dyn Host, dir: &Path) -> std::io::Result<()> {
    for name in host.read_dir(dir)? {
        let child: PathBuf = dir.join(name);
        match host.symlink_metadata(&child)?.kind {
            FileKind::Dir => remove_tree(host, &child)?,
            _ => host.remove_file(&child)?,
        }
    }
    host.remove_dir(dir)
}

/// The unit, the context copies and the pointers moi wrote — gone. Returns what was removed.
pub fn uninstall(host: &dyn Host, config: Option<&crate::config::Config>) -> Vec<String> {
    let mut done = Vec::new();
    if host.exists(Path::new(UNIT_PATH)) {
        // a service that is not running or not enabled is fine
        let _ = host.run(&Cmd::new("systemctl").args(["disable", "--now", "moi.service"]));
        if host.remove_file(Path::new(UNIT_PATH)).is_ok() {
            done.push(format!("stopped and removed {UNIT_PATH}"));
        }
        let _ = host.run(&Cmd::new("systemctl").args(["daemon-reload"]));
    }
    let mut owners: BTreeSet<u32> = BTreeSet::from([0]);
    for copy in config.map(|c| c.context.copies.clone()).unwrap_or_default() {
        if let Some(owner) = Path::new(&copy).parent().and_then(|dir| host.metadata(dir).ok()) {
            owners.insert(owner.uid);
        }
        if host.remove_file(Path::new(&copy)).is_ok() {
            done.push(format!("removed {copy}"));
        }
    }
    for uid in owners {
        if crate::context::remove_pointer(host, uid).unwrap_or(false) {
            done.push(format!("removed moi's block from the Claude Code instructions of uid {uid}"));
        }
    }
    done
}
