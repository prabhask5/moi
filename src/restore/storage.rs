//! The storage phase: which of this machine's disks holds what the old server had mounted where.
//! Nothing here can be guessed — device names differ, disks are new or were carried over — so it asks,
//! and it formats only for someone who types the device's name.

use std::path::Path;

use serde_json::Value;

use super::Run;

/// A filesystem of the old server: what `lsblk` knew about the device behind a mount.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OldDevice {
    path: String,
    uuid: String,
    label: String,
    size: u64,
    model: String,
}

/// Every device of the old machine, flattened, each with its disk's model.
fn old_devices(lsblk: &str) -> Vec<OldDevice> {
    fn walk(node: &Value, model: &str, out: &mut Vec<OldDevice>) {
        let text = |key: &str| node[key].as_str().unwrap_or_default().to_string();
        let model = if text("model").is_empty() { model.to_string() } else { text("model") };
        out.push(OldDevice {
            path: text("path"),
            uuid: text("uuid"),
            label: text("label"),
            size: node["size"].as_u64().unwrap_or(0),
            model: model.clone(),
        });
        for child in node["children"].as_array().into_iter().flatten() {
            walk(child, &model, out);
        }
    }
    let mut out = Vec::new();
    let parsed: Value = serde_json::from_str(lsblk).unwrap_or(Value::Null);
    for disk in parsed["blockdevices"].as_array().into_iter().flatten() {
        walk(disk, "", &mut out);
    }
    out
}

/// The old device an fstab source (`UUID=…`, `LABEL=…`, `/dev/…`) named.
fn old_device<'a>(devices: &'a [OldDevice], source: &str) -> Option<&'a OldDevice> {
    match (source.strip_prefix("UUID="), source.strip_prefix("LABEL=")) {
        (Some(uuid), _) => devices.iter().find(|d| d.uuid == uuid),
        (_, Some(label)) => devices.iter().find(|d| d.label == label),
        _ => devices.iter().find(|d| d.path == source),
    }
}

impl Run<'_> {
    /// The device a filesystem with this UUID is on here, if it is here.
    fn device_by_uuid(&self, uuid: &str) -> Option<String> {
        if uuid.is_empty() {
            return None;
        }
        self.look(&["blkid", "-U", uuid])
            .filter(|o| o.status == 0)
            .map(|o| o.stdout_str().trim().to_string())
            .filter(|d| !d.is_empty())
    }

    fn blkid_value(&self, device: &str, tag: &str) -> String {
        self.look(&["blkid", "-o", "value", "-s", tag, device])
            .map(|o| o.stdout_str().trim().to_string())
            .unwrap_or_default()
    }

    /// Add a line to `/etc/fstab` (or say so).
    fn fstab_line(&mut self, line: &str) {
        if self.dry() {
            self.say(&format!("    [dry-run] append to /etc/fstab: {line}"));
        } else if let Err(e) = self.host.append(Path::new("/etc/fstab"), format!("{line}\n").as_bytes(), 0o644) {
            self.warn(&format!("could not write /etc/fstab: {e}"));
        }
    }
}

pub(super) fn storage(run: &mut Run<'_>) {
    let devices = old_devices(&run.snapshot_file("storage/lsblk.json"));
    run.say("  OLD machine disks:");
    for device in devices.iter().filter(|d| !d.path.is_empty()) {
        let gb = device.size / 1_000_000_000;
        run.say(&format!(
            "    {:<18} {gb:>6} GB  {}  {}  {}",
            device.path,
            device.model,
            if device.label.is_empty() { "-" } else { &device.label },
            device.uuid
        ));
    }
    run.say("  THIS machine:");
    let here = run
        .look(&["lsblk", "-o", "NAME,SIZE,TYPE,FSTYPE,LABEL,MODEL,MOUNTPOINT", "-e7"])
        .map(|o| o.stdout_str().into_owned())
        .unwrap_or_default();
    for line in here.lines() {
        run.say(&format!("    {line}"));
    }
    // arrays and pools that were carried over are put together, not created
    if !run.snapshot_file("storage/mdadm-scan.txt").trim().is_empty()
        && run.ask("The old server had mdadm arrays. Assemble them now (only if those disks were moved over)?", false)
    {
        run.apt_install(&["mdadm"]);
        run.act(&["mdadm", "--assemble", "--scan"]);
    }
    let pools = run.snapshot_lines("storage/zpool-list.txt");
    if !pools.is_empty() && run.ask("The old server had ZFS pools. Import them (disks moved over)?", false) {
        run.apt_install(&["zfsutils-linux"]);
        for pool in pools.iter().filter_map(|l| l.split_whitespace().next()) {
            if !run.act(&["zpool", "import", "-f", pool]) {
                run.warn(&format!("could not import the pool {pool}"));
            }
        }
    }
    if !run.snapshot_file("storage/lvm.txt").trim().is_empty()
        && run.have("vgchange")
        && run.ask("Activate LVM volume groups (vgchange -ay)?", true)
    {
        run.act(&["vgchange", "-ay"]);
    }

    // the fstab as it is stays next to the new one
    if !run.dry()
        && let Ok(current) = run.host.read(Path::new("/etc/fstab"))
    {
        let kept = format!("/etc/fstab.pre-moi.{}", crate::host::unix_seconds(run.host.now()));
        let _ = run.host.write(Path::new(&kept), &current, 0o644);
    }
    let old_fstab = run.host.read_to_string(&run.files.join("etc/fstab")).unwrap_or_default();
    let now_fstab = run.host.read_to_string(Path::new("/etc/fstab")).unwrap_or_default();
    let present = |target: &str| {
        now_fstab
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .any(|l| l.split_whitespace().nth(1) == Some(target))
    };
    for line in old_fstab.lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [source, target, fstype, options, rest @ ..] = fields.as_slice() else { continue };
        let (dump, pass) = (rest.first().copied().unwrap_or("0"), rest.get(1).copied().unwrap_or("0"));
        // the system's own filesystems are the installer's business
        if ["/", "/boot", "/boot/efi", "none", "swap"].contains(target) || *fstype == "swap" {
            continue;
        }
        if present(target) {
            run.info(&format!("{target} is already in fstab here: skipped"));
            continue;
        }
        let old = old_device(&devices, source).cloned().unwrap_or_default();
        run.say("");
        run.say(&format!("  ── {target}   (old: {source}  {fstype}  {options})"));
        if !old.path.is_empty() {
            run.say(&format!("     old device: {}  {} GB  {}", old.path, old.size / 1_000_000_000, old.model));
        }
        // network and pooled filesystems name no device of this machine
        if fstype.starts_with("nfs")
            || fstype.starts_with("smb")
            || fstype.starts_with("fuse.")
            || ["cifs", "9p", "sshfs", "zfs"].contains(fstype)
        {
            if run.ask("Network or pooled mount — add this fstab line as it was?", true) {
                run.act(&["mkdir", "-p", target]);
                run.fstab_line(&format!("{source} {target} {fstype} {options} {dump} {pass}"));
            }
            continue;
        }
        // the very same filesystem is here: the disk was carried over
        if let Some(device) = run.device_by_uuid(&old.uuid) {
            run.say(&format!("     ✓ the SAME filesystem is present here ({device}) — the disk was moved over"));
            if run.ask("Mount it where it was?", true) {
                run.act(&["mkdir", "-p", target]);
                // by UUID whatever the old line said: device names are not stable across machines
                let by = if source.starts_with("UUID=") || source.starts_with("LABEL=") {
                    (*source).to_string()
                } else {
                    format!("UUID={}", old.uuid)
                };
                run.fstab_line(&format!("{by} {target} {fstype} {options} {dump} {pass}"));
                continue;
            }
        }
        if run.dry() {
            run.say(&format!("    [dry-run] would ask which device holds {target} here (formatting only after typing FORMAT <device>)"));
            continue;
        }
        let answer = loop {
            let answer = run.prompt(&format!(
                "Device for {target} here (e.g. /dev/sdb1 or UUID=…), 'l' lists the disks, Enter skips:"
            ));
            if answer != "l" {
                break answer;
            }
            for line in here.lines() {
                run.say(&format!("    {line}"));
            }
        };
        if answer.is_empty() {
            run.info(&format!("skipped {target}"));
            continue;
        }
        let device = match (answer.strip_prefix("UUID="), answer.strip_prefix("LABEL=")) {
            (Some(uuid), _) => run.device_by_uuid(uuid).unwrap_or_default(),
            (_, Some(label)) => {
                run.look(&["blkid", "-L", label]).map(|o| o.stdout_str().trim().to_string()).unwrap_or_default()
            }
            _ => answer.clone(),
        };
        if device.is_empty() || !run.host.exists(Path::new(&device)) {
            run.warn(&format!("{answer} is not a block device here: skipped {target}"));
            continue;
        }
        let mut fstype = (*fstype).to_string();
        let current = run.blkid_value(&device, "TYPE");
        if current.is_empty() {
            // an empty device: formatting erases it, so it takes the device's name typed out — `--yes`
            // answers no question here
            let label = if old.label.is_empty() { String::new() } else { format!(" (label {})", old.label) };
            let typed = run.prompt(&format!("{device} has no filesystem. Type  FORMAT {device}  to create {fstype}{label} — THIS ERASES IT — or Enter to skip:"));
            if typed != format!("FORMAT {device}") {
                run.info(&format!("skipped {target}"));
                continue;
            }
            let mut command = vec!["mkfs", "-t", fstype.as_str()];
            if !old.label.is_empty() {
                command.extend([if fstype == "vfat" { "-n" } else { "-L" }, old.label.as_str()]);
            }
            command.push(device.as_str());
            if !run.act(&command) {
                run.warn(&format!("mkfs failed on {device}: skipped {target}"));
                continue;
            }
        } else if current != fstype {
            run.warn(&format!("{device} already has {current} (the old one was {fstype})"));
            if !run.ask("Use it as it is (no formatting)?", false) {
                continue;
            }
            fstype = current;
        }
        let uuid = run.blkid_value(&device, "UUID");
        if uuid.is_empty() {
            run.warn(&format!("{device} has no UUID: skipped {target}"));
            continue;
        }
        // a data disk that is missing must not keep the machine from booting
        let options = if options.split(',').any(|o| o == "nofail") {
            (*options).to_string()
        } else {
            format!("{options},nofail")
        };
        run.act(&["mkdir", "-p", target]);
        run.fstab_line(&format!("UUID={uuid} {target} {fstype} {options} {dump} {pass}"));
    }
    run.act(&["systemctl", "daemon-reload"]);
    if !run.act(&["mount", "-a"]) {
        run.warn("mount -a reported errors: check /etc/fstab");
    }
}
