//! § 6 — Storage placement: is each container's data on the right kind of disk? A port of the owner's
//! `disk-audit.sh`: the flags are keyword-based, exactly as there, and meant to be read with judgment.

use std::path::Path;

use serde_json::Value;

use super::{Live, Section, human};
use crate::backup::under;
use crate::config::Config;
use crate::host::FileKind;
use crate::snapshot::View;

/// (type, source, destination)
type Mount = (String, String, String);

/// Mounts that are plumbing, never data.
const SKIP: [&str; 7] = ["docker.sock", "/etc/localtime", "/etc/timezone", "/dev/", "/proc", "/sys", "/run/"];

/// A keyword matches anywhere in the lower-cased `source destination`; one ending in `$` must be
/// followed by `/` or end the text (the script's `($|/)`).
pub(crate) fn matches(keywords: &[String], text: &str) -> bool {
    keywords.iter().any(|keyword| match keyword.strip_suffix('$') {
        Some(k) => text.match_indices(k).any(|(i, _)| {
            let rest = &text[i + k.len()..];
            rest.is_empty() || rest.starts_with('/')
        }),
        None => text.contains(keyword.as_str()),
    })
}

/// `SSD`, `HDD`, `RAM` or `?` for the device a host path is on.
pub(crate) fn disk_class(view: &View<'_>, path: &str) -> &'static str {
    // the mount the path is on: longest matching target
    let mounts = view.lines("storage/mounts.txt");
    let mount = mounts
        .iter()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?, f.next()?, f.next()?))
        })
        .filter(|(target, _, _)| under(path, target))
        .max_by_key(|(target, _, _)| target.len());
    let Some((_, source, fstype)) = mount else { return "?" };
    if ["tmpfs", "shm", "overlay", "ramfs"].contains(&fstype) {
        return "RAM";
    }
    // the disk that device belongs to, and whether it spins
    fn find(devices: &Value, source: &str, inherited: Option<bool>) -> Option<Option<bool>> {
        for d in devices.as_array().into_iter().flatten() {
            let rota = d["rota"].as_bool().or(inherited);
            if d["path"] == source
                || d["name"]
                    .as_str()
                    .is_some_and(|n| format!("/dev/{n}") == source || format!("/dev/mapper/{n}") == source)
            {
                return Some(rota);
            }
            if let Some(found) = find(&d["children"], source, rota) {
                return Some(found);
            }
        }
        None
    }
    match find(&view.json("storage/lsblk.json")["blockdevices"], source, None) {
        Some(Some(true)) => "HDD",
        Some(Some(false)) => "SSD",
        _ => "?",
    }
}

pub(super) fn section(view: &View<'_>, config: &Config, live: &Live) -> Section {
    let mut s = Section::new("placement", "Storage placement");
    let placement = &config.audit.placement;
    let root = view.text("docker/root-dir").trim().to_string();
    if !root.is_empty() {
        s.info(format!("Docker data root `{root}` is on {}", disk_class(view, &root)));
    }
    let states = view.container_states();
    // (name, mounts as (type, source, destination)) of running containers and of swarm services
    let mut workloads: Vec<(String, Vec<Mount>)> = Vec::new();
    for c in view.containers() {
        let name = c["name"].as_str().unwrap_or_default().to_string();
        if states.get(&name).is_some_and(|state| state != "running") {
            continue;
        }
        let mounts = c["mounts"].as_array().into_iter().flatten().map(|m| {
            let text = |key: &str| m[key].as_str().unwrap_or_default().to_string();
            (text("type"), if text("type") == "volume" { text("name") } else { text("source") }, text("destination"))
        });
        workloads.push((name, mounts.collect()));
    }
    for service in view.services() {
        let mounts = service["mounts"].as_array().into_iter().flatten().map(|m| {
            let text = |key: &str| m[key].as_str().unwrap_or_default().to_string();
            (text("type"), text("source"), text("destination"))
        });
        workloads.push((service["name"].as_str().unwrap_or_default().to_string(), mounts.collect()));
    }
    let docker_root = if root.is_empty() { "/var/lib/docker" } else { root.as_str() };
    let mut rows = Vec::new();
    for (name, mounts) in &workloads {
        let mut bulk_mounts = 0;
        for (kind, source, destination) in mounts {
            // anonymous volumes have no name in the snapshot and nothing to place
            if source.is_empty() {
                continue;
            }
            let source =
                if kind == "volume" { format!("{docker_root}/volumes/{source}/_data") } else { source.clone() };
            if SKIP.iter().any(|skip| format!("{source}{destination}").contains(skip)) {
                continue;
            }
            // single files are configuration, not where data piles up
            if view.host().metadata(Path::new(&source)).is_ok_and(|md| md.kind == FileKind::File) {
                continue;
            }
            let class = disk_class(view, &source);
            let lower = format!("{source} {destination}").to_lowercase();
            let mut flag = "";
            if matches(&placement.bulk, &lower) {
                bulk_mounts += 1;
                if class == "SSD" {
                    flag = "bulk data on SSD";
                    s.review(format!("**{name}** `{destination}` ← `{source}`: bulk data on SSD"));
                }
            } else if matches(&placement.hot, &lower) && class == "HDD" {
                flag = "database/config on HDD";
                s.gap(format!("**{name}** `{destination}` ← `{source}`: database/config on HDD"));
            }
            rows.push(vec![
                name.clone(),
                class.to_string(),
                kind.clone(),
                destination.clone(),
                source,
                flag.to_string(),
            ]);
        }
        if bulk_mounts > 1 {
            s.review(format!("**{name}** has {bulk_mounts} separate bulk mounts: hardlinks between them cannot work — use one `/data` mount"));
        }
        if let Some(bytes) = live.size_rw.get(name).filter(|b| **b > 1 << 30) {
            s.gap(format!("**{name}** has {} written inside the container itself (not in a volume)", human(*bytes)));
        }
    }
    s.table(&["Container", "Disk", "Type", "Mounted at", "Source", "Flag"], rows);
    if s.blocks
        .iter()
        .any(|b| matches!(b, super::Block::Finding { level: super::Level::Gap | super::Level::Review, .. }))
    {
        s.text("_Flags are keyword-based (`[audit.placement]` hot / bulk): use judgment._");
    }
    s.or_nothing("no containers to place")
}
