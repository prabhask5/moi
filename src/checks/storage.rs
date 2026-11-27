//! `storage`: SMART health, disk errors in the kernel log, space, inodes, read-only filesystems,
//! writable mounts. A port of `storage-check.sh`; the message is the script's, byte for byte.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::{Check, Cx, Memory, Outcome, kernel_log_matches, with_timeout};
use crate::config::{AutoOr, Config, Disk};
use crate::host::{Cmd, Host};
use crate::snapshot::mounts;

pub(crate) struct Storage;

/// How long a filesystem may take to answer before it counts as failed (a hung USB disk blocks forever).
const FS_TIMEOUT: Duration = Duration::from_secs(10);
const SMART_TIMEOUT: Duration = Duration::from_secs(30);

/// Physical disks from `/sys/block`, for `disks = "auto"`.
fn discover(host: &dyn Host) -> Vec<Disk> {
    host.read_dir(Path::new("/sys/block"))
        .unwrap_or_default()
        .into_iter()
        .filter(|name| !["loop", "ram", "zram", "dm-", "sr", "md", "fd", "nbd"].iter().any(|p| name.starts_with(p)))
        .map(|name| Disk {
            kind: if name.starts_with("nvme") { "nvme" } else { "auto" }.to_string(),
            device: format!("/dev/{name}"),
            temp_max: None,
        })
        .collect()
}

/// The leading integer of a raw SMART value (`31 (Min/Max 20/45)` → 31), like awk's numeric `$10`.
fn leading_number(raw: &str) -> Option<u64> {
    let token = raw.split_whitespace().next()?;
    token.bytes().all(|b| b.is_ascii_digit()).then(|| token.parse().ok()).flatten()
}

/// Lines of the kernel log that mean a disk or its USB link is in trouble.
fn disk_error(line: &str) -> bool {
    [
        "i/o error",
        "ext4-fs error",
        "buffer i/o",
        "blk_update_request",
        "critical medium error",
        "uas_eh",
        "reset superspeed",
        "device offline",
    ]
    .iter()
    .any(|p| line.contains(p))
        || line.find("xfs ").is_some_and(|i| line[i..].contains("error"))
}

impl Check for Storage {
    fn name(&self) -> String {
        "storage".to_string()
    }

    fn timeout(&self, config: &Config) -> Duration {
        config.checks.storage.common.timeout.as_duration()
    }

    fn interval(&self, config: &Config) -> Duration {
        config.checks.storage.common.interval.as_duration()
    }

    fn monitor(&self, config: &Config) -> Option<String> {
        config.checks.storage.common.monitor.clone()
    }

    fn journal_tag(&self, config: &Config) -> String {
        config.checks.storage.common.journal_tag.clone()
    }

    fn run(&self, cx: &Cx, _memory: &mut Memory) -> Outcome {
        let settings = &cx.config.checks.storage;
        let host = cx.host.as_ref();
        let (mut bad, mut info) = (String::new(), String::new());

        // 1. SMART health of each disk
        let disks = match &settings.disks {
            AutoOr::List(disks) => disks.clone(),
            AutoOr::Auto => discover(host),
        };
        for disk in &disks {
            let name = disk.device.rsplit('/').next().unwrap_or(&disk.device);
            if !host.exists(Path::new(&disk.device)) {
                bad.push_str(&format!("{name} (disk missing) "));
                continue;
            }
            let cmd = Cmd::new("smartctl")
                .args(["--json", "-H", "-A", "-d", disk.kind.as_str(), disk.device.as_str()])
                .timeout(SMART_TIMEOUT);
            let out = match host.run(&cmd) {
                Ok(out) if out.timed_out => {
                    bad.push_str(&format!("{name} (SMART timed out) "));
                    continue;
                }
                Ok(out) => out,
                // no smartctl is how the script ended up here too (exit 127 has the "unreadable" bit set)
                Err(_) => {
                    bad.push_str(&format!("{name} (SMART unreadable) "));
                    continue;
                }
            };
            // smartctl's exit status is a bit mask
            if out.status & 2 != 0 {
                bad.push_str(&format!("{name} (SMART unreadable) "));
                continue;
            }
            if out.status & 8 != 0 {
                bad.push_str(&format!("{name} (SMART says FAILING) "));
            }
            if out.status & 16 != 0 {
                bad.push_str(&format!("{name} (SMART attribute past failure threshold) "));
            }
            let report: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
            let nvme = &report["nvme_smart_health_information_log"];
            let temp = if disk.kind == "nvme" || nvme.is_object() {
                if let Some(warning) = nvme["critical_warning"].as_u64().filter(|w| *w != 0) {
                    bad.push_str(&format!("{name} (critical warning 0x{warning:02x}) "));
                }
                if let Some(used) = nvme["percentage_used"].as_u64() {
                    info.push_str(&format!("{name} {used}% worn, "));
                    if used >= u64::from(settings.nvme_wear_limit) {
                        bad.push_str(&format!("{name} ({used}% of rated life used) "));
                    }
                }
                if let Some(errors) = nvme["media_errors"].as_u64().filter(|e| *e > 0) {
                    bad.push_str(&format!("{name} ({errors} media errors) "));
                }
                nvme["temperature"].as_u64().or_else(|| report["temperature"]["current"].as_u64())
            } else {
                // 5 reallocated, 197 pending, 198 offline-uncorrectable sectors; 194/190 temperature
                let table: Vec<&Value> =
                    report["ata_smart_attributes"]["table"].as_array().into_iter().flatten().collect();
                let raw = |attribute: &Value| leading_number(attribute["raw"]["string"].as_str().unwrap_or_default());
                let sectors: u64 = table
                    .iter()
                    .filter(|a| [5, 197, 198].contains(&a["id"].as_u64().unwrap_or(0)))
                    .filter_map(|a| raw(a))
                    .sum();
                if sectors > settings.allowed_bad_sectors {
                    bad.push_str(&format!("{name} ({sectors} bad/pending sectors) "));
                }
                table.iter().find(|a| [194, 190].contains(&a["id"].as_u64().unwrap_or(0))).and_then(|a| raw(a))
            };
            if let Some(temp) = temp {
                info.push_str(&format!("{name} {temp}C, "));
                let limit = disk.temp_max.unwrap_or(if disk.kind == "nvme" { 70 } else { 55 });
                if temp >= u64::from(limit) {
                    bad.push_str(&format!("{name} ({temp}C, too hot) "));
                }
            }
        }

        // 2. Disk and USB errors in the kernel log since the last run
        let errors = kernel_log_matches(host, settings.kernel_error_window.as_duration(), disk_error);
        if errors > 0 {
            let minutes = settings.kernel_error_window.as_secs() / 60;
            bad.push_str(&format!(
                "kernel log ({errors} disk/USB errors in last {minutes} min, see: journalctl -k --since -10min) "
            ));
        }

        // 3. Space, inodes, read-only
        let mounted = mounts(host);
        for mount in &settings.mounts {
            let (probe_host, path) = (Arc::clone(&cx.host), PathBuf::from(mount));
            let usage = with_timeout(FS_TIMEOUT, move || probe_host.statvfs(&path)).and_then(Result::ok);
            // df's percentages: used / (used + available to users), rounded up
            let percent = |used: u64, total: u64| (total > 0).then(|| (used * 100).div_ceil(total));
            let Some((usage, used_percent)) = usage.and_then(|u| {
                let used = u.blocks.saturating_sub(u.blocks_free);
                Some((u, percent(used, used + u.blocks_available)?))
            }) else {
                bad.push_str(&format!("{mount} (df failed) "));
                continue;
            };
            info.push_str(&format!("{mount} {used_percent}% used, "));
            if used_percent >= u64::from(settings.disk_limit) {
                bad.push_str(&format!("{mount} ({used_percent}% full) "));
            }
            if let Some(inodes) = percent(usage.inodes.saturating_sub(usage.inodes_free), usage.inodes)
                .filter(|p| *p >= u64::from(settings.inode_limit))
            {
                bad.push_str(&format!("{mount} ({inodes}% of inodes used) "));
            }
            // the filesystem the path is on: the longest mount point containing it
            let on = mounted.iter().filter(|m| crate::backup::under(mount, &m.target)).max_by_key(|m| m.target.len());
            if on.is_some_and(|m| m.options.split(',').chain(m.super_options.split(',')).any(|o| o == "ro")) {
                bad.push_str(&format!("{mount} (read-only) "));
            }
        }

        // 4. Mounts that must be there and writable (last: a hung USB disk can block)
        for mount in &settings.writable {
            if !mounted.iter().any(|m| m.target == *mount) {
                bad.push_str(&format!("{mount} (not mounted) "));
                continue;
            }
            let (probe_host, probe) = (Arc::clone(&cx.host), Path::new(mount).join(".storage-check"));
            let wrote = with_timeout(FS_TIMEOUT, move || {
                let result = probe_host.write(&probe, b"", 0o644);
                if result.is_ok() {
                    let _ = probe_host.remove_file(&probe);
                }
                result.is_ok()
            });
            if wrote != Some(true) {
                bad.push_str(&format!("{mount} (not writable) "));
            }
        }

        if bad.is_empty() {
            Outcome::up(format!("OK: {}", info.strip_suffix(", ").unwrap_or(&info)))
        } else {
            Outcome::down(format!("Problem: {bad}"))
        }
    }
}
