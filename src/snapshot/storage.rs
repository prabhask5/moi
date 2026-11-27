//! Disks, filesystems and mounts, plus the data roots whose directory structure `trees` records.
//!
//! Mounts come straight from `/proc/self/mountinfo`; block devices and volume managers from their own
//! tools when installed (`lsblk`, `blkid`, `mdadm`, LVM, ZFS, btrfs, `smartctl`).

use std::collections::BTreeSet;
use std::path::Path;

use super::collect::lines;
use super::{Collector, Ctx, Output, run_tool};
use crate::config::AutoOr;
use crate::host::{FileKind, Host};

pub(crate) struct Storage;

/// Filesystem types that are kernel interfaces or container plumbing, not storage.
const PSEUDO_FS: [&str; 29] = [
    "overlay",
    "tmpfs",
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devpts",
    "mqueue",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "configfs",
    "fusectl",
    "hugetlbfs",
    "autofs",
    "nsfs",
    "devtmpfs",
    "squashfs",
    "efivarfs",
    "binfmt_misc",
    "ramfs",
    "rpc_pipefs",
    "fuse.lxcfs",
    "fuse.gvfsd-fuse",
    "fuse.portal",
    "selinuxfs",
    "fuse.snapfuse",
];

/// Filesystem types that hold a server's data.
const DATA_FS: [&str; 17] = [
    "ext2",
    "ext3",
    "ext4",
    "xfs",
    "btrfs",
    "zfs",
    "f2fs",
    "ntfs",
    "ntfs3",
    "exfat",
    "vfat",
    "fuse.mergerfs",
    "fuse.shfs",
    "nfs",
    "nfs4",
    "cifs",
    "bcachefs",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mount {
    pub target: String,
    pub source: String,
    pub fstype: String,
    pub options: String,
}

/// Parse `/proc/self/mountinfo` (`id parent maj:min root target options [tags…] - fstype source superopts`).
pub(crate) fn mounts(host: &dyn Host) -> Vec<Mount> {
    let text = host.read_to_string(Path::new("/proc/self/mountinfo")).unwrap_or_default();
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let left: Vec<&str> = left.split(' ').collect();
        let right: Vec<&str> = right.split(' ').collect();
        if let (Some(target), Some(options), Some(fstype), Some(source)) =
            (left.get(4), left.get(5), right.first(), right.get(1))
        {
            out.push(Mount {
                target: unescape(target),
                source: unescape(source),
                fstype: (*fstype).to_string(),
                options: (*options).to_string(),
            });
        }
    }
    out
}

/// mountinfo writes space, tab, newline and backslash as `\040`-style octal escapes.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            let value = bytes[i + 1..i + 4].iter().fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
            out.push(u8::try_from(value).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The directories whose structure is recorded: `capture.data_roots`, or discovered — every mounted data
/// filesystem that is not part of the system itself, plus the usual top-level data directories.
pub(crate) fn data_roots(cx: &Ctx) -> Vec<String> {
    if let AutoOr::List(roots) = &cx.config.capture.data_roots {
        return roots.clone();
    }
    let host = cx.host.as_ref();
    let system = |t: &str| {
        t == "/"
            || t == "/var"
            || t == "/tmp"
            || t == "/var/log"
            || ["/boot", "/var/lib/docker", "/snap", "/run", "/usr"]
                .iter()
                .any(|p| t == *p || t.starts_with(&format!("{p}/")))
            || t.starts_with("/boot")
    };
    let mut roots: BTreeSet<String> = mounts(host)
        .into_iter()
        .filter(|m| DATA_FS.contains(&m.fstype.as_str()) && !system(&m.target))
        .map(|m| m.target)
        .collect();
    for dir in ["/srv", "/opt", "/data", "/home", "/mnt", "/media"] {
        if host.metadata(Path::new(dir)).is_ok_and(|m| m.kind == FileKind::Dir) {
            roots.insert(dir.to_string());
        }
    }
    roots.into_iter().collect()
}

impl Collector for Storage {
    fn name(&self) -> &'static str {
        "storage"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let mut out = Output::default();
        let lsblk = run_tool(
            host,
            "lsblk",
            &[
                "-J",
                "-b",
                "-o",
                "NAME,PATH,SIZE,TYPE,FSTYPE,LABEL,UUID,PARTUUID,MOUNTPOINT,MODEL,SERIAL,ROTA,TRAN",
                "-e",
                "7",
            ],
        );
        out.stable("storage/lsblk.json", lsblk.unwrap_or_default());

        let mut rows: Vec<String> = mounts(host)
            .into_iter()
            .filter(|m| !PSEUDO_FS.contains(&m.fstype.as_str()))
            .filter(|m| !["/var/lib/docker/", "/run/", "/snap/"].iter().any(|p| m.target.starts_with(p)))
            .map(|m| format!("{} {} {} {}", m.target, m.source, m.fstype, m.options))
            .collect();
        rows.sort();
        rows.dedup();
        out.stable("storage/mounts.txt", lines(rows));

        out.stable("storage/blkid.txt", run_tool(host, "blkid", &[]).unwrap_or_default());
        if let Some(scan) = run_tool(host, "mdadm", &["--detail", "--scan"]) {
            out.stable("storage/mdadm-scan.txt", scan);
            out.volatile("mdstat.txt", host.read_to_string(Path::new("/proc/mdstat")).unwrap_or_default());
        }
        if let Some(pvs) = run_tool(host, "pvs", &[]) {
            let vgs = run_tool(host, "vgs", &[]).unwrap_or_default();
            let lvs = run_tool(host, "lvs", &[]).unwrap_or_default();
            out.stable("storage/lvm.txt", format!("{pvs}\n{vgs}\n{lvs}"));
        }
        if let Some(pools) = run_tool(host, "zpool", &["list", "-H", "-o", "name,size,health"]) {
            out.stable("storage/zpool-list.txt", pools);
            out.stable(
                "storage/zfs-datasets.txt",
                run_tool(host, "zfs", &["list", "-H", "-o", "name,mountpoint"]).unwrap_or_default(),
            );
            out.volatile("zpool-status.txt", run_tool(host, "zpool", &["status"]).unwrap_or_default());
        }
        let mut subvolumes = String::new();
        for m in mounts(host).iter().filter(|m| m.fstype == "btrfs") {
            if let Some(list) = run_tool(host, "btrfs", &["subvolume", "list", &m.target]) {
                subvolumes.push_str(&format!("== {}\n{list}", m.target));
            }
        }
        out.stable("storage/btrfs-subvolumes.txt", subvolumes);

        out.volatile(
            "df.txt",
            run_tool(
                host,
                "df",
                &["-hT", "-x", "tmpfs", "-x", "devtmpfs", "-x", "overlay", "-x", "squashfs", "-x", "efivarfs"],
            )
            .unwrap_or_default(),
        );
        if let Some(scan) = run_tool(host, "smartctl", &["--scan"]) {
            let mut smart = String::new();
            for device in scan.lines().filter_map(|l| l.split_whitespace().next()) {
                // smartctl's exit status is a bit mask of findings, so read stdout whatever it returned
                let cmd = crate::host::Cmd::new("smartctl")
                    .args(["-i", "-H", device])
                    .timeout(std::time::Duration::from_secs(20));
                if let Ok(result) = host.run(&cmd) {
                    smart.push_str(&format!("== {device}\n"));
                    let text = result.stdout_str().into_owned();
                    for line in text.lines().filter(|l| {
                        ["Model", "Serial", "Capacity", "Rotation", "Form Factor", "overall-health", "SMART Health"]
                            .iter()
                            .any(|k| l.contains(k))
                    }) {
                        smart.push_str(line);
                        smart.push('\n');
                    }
                }
            }
            out.volatile("smart.txt", smart);
        }
        out.stable("storage/data-roots.txt", lines(data_roots(cx)));
        Ok(out)
    }
}
