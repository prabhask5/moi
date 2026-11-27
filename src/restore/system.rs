//! The phases that rebuild the operating system side: packages, users, files under `/etc` and in
//! home directories, units and cron jobs.

use std::collections::BTreeSet;
use std::path::Path;

use super::Run;
use crate::host::Cmd;

/// `KEY="value"` from an os-release file.
fn os_field(text: &str, key: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_default()
}

pub(super) fn preflight(run: &mut Run<'_>) {
    let old_os = run.snapshot_file("meta/os-release");
    let new_os = run.host.read_to_string(Path::new("/etc/os-release")).unwrap_or_default();
    let (old_name, new_name) = (os_field(&old_os, "PRETTY_NAME"), os_field(&new_os, "PRETTY_NAME"));
    let old_host = run.snapshot_file("meta/hostname").trim().to_string();
    let here = crate::host::short_hostname(run.host.as_ref());
    run.say(&format!("  Source machine : {old_host}  captured {}", run.captured_at()));
    run.say(&format!("  Source OS      : {old_name}"));
    run.say(&format!("  This machine   : {here} — {new_name}"));
    if old_name != new_name {
        run.warn("the operating system differs: package names and repositories may not match exactly; review the packages phase");
    }
    // architecture and memory: what will not fit should be known before anything is installed
    let arch = |cpu: &str| {
        cpu.lines().find_map(|l| l.strip_prefix("Architecture:")).map(|a| a.trim().to_string()).unwrap_or_default()
    };
    let old_arch = arch(&run.snapshot_file("meta/cpu"));
    let new_arch = run.look(&["uname", "-m"]).map(|o| o.stdout_str().trim().to_string()).unwrap_or_default();
    if !old_arch.is_empty() && !new_arch.is_empty() && old_arch != new_arch {
        run.warn(&format!(
            "the CPU architecture differs ({old_arch} → {new_arch}): container images must exist for {new_arch}"
        ));
    }
    let containers = serde_json::from_str::<serde_json::Value>(&run.snapshot_file("docker/containers.json"))
        .ok()
        .and_then(|v| v.as_array().map(Vec::len))
        .unwrap_or(0);
    let stacks = serde_json::from_str::<serde_json::Value>(&run.snapshot_file("docker/compose-projects.json"))
        .ok()
        .and_then(|v| v.as_array().map(Vec::len))
        .unwrap_or(0);
    let units = run.snapshot_lines("systemd/custom-units.txt").iter().filter(|u| !u.ends_with(".conf")).count();
    run.say(&format!("  Containers: {containers}  stacks: {stacks}  custom units: {units}"));
    if run.have("apt-get") {
        run.act(&["apt-get", "update", "-q"]);
        run.apt_install(&["curl", "ca-certificates", "gnupg", "restic"]);
    }
}

/// Packages that belong to the machine, not to the server's setup: never installed from a list.
const MACHINE_PACKAGES: [&str; 14] = [
    "linux-image",
    "linux-headers",
    "linux-modules",
    "linux-generic",
    "linux-virtual",
    "linux-firmware",
    "linux-signed",
    "linux-hwe",
    "grub",
    "shim",
    "efibootmgr",
    "intel-microcode",
    "amd64-microcode",
    "cloud-",
];

pub(super) fn packages(run: &mut Run<'_>) {
    let family = run.snapshot_file("packages/family").trim().to_string();
    if family == "debian" && run.have("apt-get") {
        let codename = |text: &str| os_field(text, "VERSION_CODENAME");
        let old = codename(&run.snapshot_file("meta/os-release"));
        let new = codename(&run.host.read_to_string(Path::new("/etc/os-release")).unwrap_or_default());
        let same = old == new;
        if same
            || run
                .ask(&format!("The old release '{old}' is not this one ('{new}'). Copy the apt sources anyway?"), false)
        {
            for path in
                ["/etc/apt/sources.list.d", "/etc/apt/keyrings", "/etc/apt/trusted.gpg.d", "/etc/apt/preferences.d"]
            {
                run.restore_path(path, false);
            }
            run.restore_path("/usr/share/keyrings", true);
            if same {
                run.restore_path("/etc/apt/sources.list", false);
            }
        } else {
            run.info("kept this machine's apt sources; the old third-party repositories are in the bundle under files/etc/apt/sources.list.d");
        }
        if !run.act(&["apt-get", "update", "-q"]) {
            run.warn("apt-get update reported errors (a third-party repository may need attention)");
        }
        let wanted: BTreeSet<String> = run
            .snapshot_lines("packages/apt-manual.txt")
            .into_iter()
            .filter(|p| !MACHINE_PACKAGES.iter().any(|m| p.starts_with(m)))
            .collect();
        // what this machine's repositories do not have is listed, not fatal
        let available: Option<BTreeSet<String>> = run
            .look(&["apt-cache", "pkgnames"])
            .filter(|o| o.status == 0)
            .map(|o| o.stdout_str().lines().map(str::to_string).collect());
        let (install, missing): (Vec<String>, Vec<String>) = match &available {
            Some(available) => wanted.iter().cloned().partition(|p| available.contains(p)),
            None => (wanted.iter().cloned().collect(), Vec::new()),
        };
        run.info(&format!("installing {} apt packages", install.len()));
        let names: Vec<&str> = install.iter().map(String::as_str).collect();
        if !names.is_empty() && !run.apt_install(&names) {
            run.warn("installing them together failed: trying one by one");
            for name in &names {
                if !run.apt_install(&[name]) {
                    run.warn(&format!("  could not install {name}"));
                }
            }
        }
        if !missing.is_empty() {
            run.warn(&format!("not available in this machine's repositories: {}", missing.join(" ")));
        }
        let holds = run.snapshot_lines("packages/apt-hold.txt");
        let holds: Vec<&str> =
            holds.iter().map(String::as_str).filter(|h| !MACHINE_PACKAGES.iter().any(|m| h.starts_with(m))).collect();
        if !holds.is_empty() {
            let mut command = vec!["apt-mark", "hold"];
            command.extend(holds);
            run.act(&command);
        }
    } else if family == "debian" {
        run.warn("the old server used apt and this machine has none: packages are listed in the bundle under snapshot/packages");
    }
    // snaps (never the runtime ones snapd brings itself)
    if run.have("snap") {
        for line in run.snapshot_lines("packages/snap.txt") {
            let mut cells = line.split('\t');
            let (name, channel, notes) =
                (cells.next().unwrap_or_default(), cells.next().unwrap_or_default(), cells.next().unwrap_or_default());
            if name.is_empty()
                || name.starts_with("core")
                || ["snapd", "bare", "gtk-common-themes"].contains(&name)
                || name.starts_with("gnome-")
            {
                continue;
            }
            let channel_arg = format!("--channel={channel}");
            let mut command = vec!["snap", "install", name];
            if !channel.is_empty() {
                command.push(&channel_arg);
            }
            if notes.contains("classic") {
                command.push("--classic");
            }
            if !run.act(&command) {
                run.warn(&format!("snap {name} failed"));
            }
        }
    }
    let pipx = run.snapshot_lines("packages/pipx.txt");
    if !pipx.is_empty() {
        if !run.have("pipx") {
            run.apt_install(&["pipx"]);
        }
        for line in &pipx {
            if let Some(package) = line.split_whitespace().next() {
                run.act(&["pipx", "install", package]);
            }
        }
    }
    let npm: Vec<String> =
        run.snapshot_lines("packages/npm-global.txt").into_iter().filter(|p| p != "npm" && p != "corepack").collect();
    if !npm.is_empty() && (run.have("npm") || run.dry()) {
        let mut command = vec!["npm", "install", "-g"];
        command.extend(npm.iter().map(String::as_str));
        run.act(&command);
    }
    // binaries someone put into /usr/local/bin by hand cannot come from anywhere but their owner
    let manual: Vec<String> = run
        .snapshot_lines("packages/usr-local-bin.tsv")
        .iter()
        .filter_map(|l| {
            let (path, kind) = l.split_once('\t')?;
            (!kind.contains("script") && !kind.contains("text") && !kind.contains("symbolic") && !run.captured(path))
                .then(|| path.rsplit('/').next().unwrap_or(path).to_string())
        })
        .collect();
    if !manual.is_empty() {
        run.warn(&format!(
            "installed by hand on the old server and not captured — reinstall yourself: {}",
            manual.join(" ")
        ));
    }
}

/// name:x:id:… lines of this machine.
fn local(run: &Run<'_>, file: &str) -> Vec<Vec<String>> {
    run.host
        .read_to_string(Path::new(file))
        .unwrap_or_default()
        .lines()
        .map(|l| l.split(':').map(str::to_string).collect())
        .collect()
}

pub(super) fn users(run: &mut Run<'_>) {
    let old_groups: Vec<Vec<String>> =
        run.snapshot_lines("users/groups.group").iter().map(|l| l.split(':').map(str::to_string).collect()).collect();
    let old_users: Vec<Vec<String>> =
        run.snapshot_lines("users/users.passwd").iter().map(|l| l.split(':').map(str::to_string).collect()).collect();
    let field = |row: &[String], i: usize| row.get(i).cloned().unwrap_or_default();
    // groups of people (system groups come with their packages), with their old ids where free
    let mut groups = local(run, "/etc/group");
    for group in old_groups.iter().filter(|g| field(g, 2).parse::<u32>().is_ok_and(|gid| gid >= 1000)) {
        let (name, gid) = (field(group, 0), field(group, 2));
        if groups.iter().any(|g| field(g, 0) == name) {
            continue;
        }
        if groups.iter().any(|g| field(g, 2) == gid) {
            run.warn(&format!("gid {gid} is taken here: creating group {name} with a new one"));
            run.act(&["groupadd", &name]);
        } else {
            run.act(&["groupadd", "-g", &gid, &name]);
        }
        groups.push(vec![name, "x".into(), gid]);
    }
    let shadow = run.host.read_to_string(&run.files.join("etc/shadow")).unwrap_or_default();
    let mut users = local(run, "/etc/passwd");
    for user in &old_users {
        let (name, uid, gid, gecos, home, shell) =
            (field(user, 0), field(user, 2), field(user, 3), field(user, 4), field(user, 5), field(user, 6));
        if uid == "0" || name.is_empty() {
            continue;
        }
        match users.iter().find(|u| field(u, 0) == name) {
            Some(existing) => {
                if field(existing, 2) != uid {
                    run.warn(&format!(
                        "user {name} exists here with uid {} (was {uid}): file ownership may need a chown",
                        field(existing, 2)
                    ));
                }
            }
            None => {
                if users.iter().any(|u| field(u, 2) == uid) {
                    run.warn(&format!("uid {uid} is taken here: skipping user {name}"));
                    continue;
                }
                if !groups.iter().any(|g| field(g, 2) == gid) {
                    run.act(&["groupadd", "-g", &gid, &name]);
                    groups.push(vec![name.clone(), "x".into(), gid.clone()]);
                }
                run.act(&["useradd", "-u", &uid, "-g", &gid, "-d", &home, "-m", "-s", &shell, "-c", &gecos, &name]);
                users.push(user.clone());
            }
        }
        // the password they had (a locked account stays locked)
        let hash = shadow
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name}:")))
            .and_then(|rest| rest.split(':').next())
            .unwrap_or_default();
        if !hash.is_empty() && !hash.starts_with('!') && hash != "*" {
            // on standard input: a hash on a command line is in the process list and in logs
            if !run.act_cmd(Cmd::new("chpasswd").arg("-e").stdin(format!("{name}:{hash}\n").into_bytes())) {
                run.warn(&format!("could not set the password of {name}"));
            }
        }
    }
    // memberships. A system group whose package comes later (docker, incus-admin) is created now: the
    // package adopts a group that exists
    for group in &old_groups {
        let name = field(group, 0);
        let members = field(group, 3);
        if !groups.iter().any(|g| field(g, 0) == name) {
            if !members.split(',').any(|m| users.iter().any(|u| field(u, 0) == m))
                || !run.act(&["groupadd", "-r", &name])
            {
                continue;
            }
            groups.push(vec![name.clone(), "x".into(), String::new()]);
        }
        let already: Vec<String> = local(run, "/etc/group")
            .iter()
            .find(|g| field(g, 0) == name)
            .map(|g| field(g, 3).split(',').map(str::to_string).collect())
            .unwrap_or_default();
        for member in field(group, 3).split(',').filter(|m| !m.is_empty()) {
            if users.iter().any(|u| field(u, 0) == member) && !already.iter().any(|a| a == member) {
                run.act(&["usermod", "-aG", &name, member]);
            }
        }
    }
}

pub(super) fn home(run: &mut Run<'_>) {
    for line in run.snapshot_lines("users/users.passwd") {
        let cells: Vec<&str> = line.split(':').collect();
        let (Some(name), Some(home)) = (cells.first(), cells.get(5)) else { continue };
        if run.captured(home) {
            run.info(&format!("{name}: {home}"));
            run.restore_path(home, false);
        }
    }
}

pub(super) fn dirs(run: &mut Run<'_>) {
    let trees = run.vol.join("trees");
    let (users, groups) = (run.ids("/etc/passwd"), run.ids("/etc/group"));
    for file in run.host.read_dir(&trees).unwrap_or_default() {
        let rows: Vec<Vec<String>> = run
            .host
            .read_to_string(&trees.join(&file))
            .unwrap_or_default()
            .lines()
            .map(|l| l.split('\t').map(str::to_string).collect())
            .filter(|r: &Vec<String>| r.len() == 4)
            .collect();
        let Some(root) = rows.first().map(|r| r[3].clone()) else { continue };
        if !run.host.exists(Path::new(&root)) && run.dry() {
            run.info(&format!(
                "[dry-run] would create {} directories under {root} (it does not exist here yet)",
                rows.len()
            ));
            continue;
        }
        if !run.host.exists(Path::new(&root)) {
            run.warn(&format!("{root} does not exist here (mount it first): its directory tree is skipped"));
            continue;
        }
        let missing: Vec<&Vec<String>> = rows.iter().filter(|r| !run.host.exists(Path::new(&r[3]))).collect();
        if missing.is_empty() {
            run.ok(&format!("{root}: structure already complete"));
            continue;
        }
        if !run.ask(
            &format!("Recreate {} missing directories under {root} (with their owners and modes)?", missing.len()),
            true,
        ) {
            continue;
        }
        if run.dry() {
            run.info(&format!("[dry-run] would create {} directories under {root}", missing.len()));
            continue;
        }
        let mut created = 0;
        for row in &missing {
            let path = Path::new(&row[3]);
            let mode = u32::from_str_radix(&row[0], 8).unwrap_or(0o755);
            if run.host.create_dir_all(path, mode).is_ok() {
                let _ = run.host.set_mode(path, mode);
                let _ = run.host.set_owner(
                    path,
                    users.get(&row[1]).copied().unwrap_or(0),
                    groups.get(&row[2]).copied().unwrap_or(0),
                );
                created += 1;
            }
        }
        run.ok(&format!("{root}: created {created} directories"));
    }
}

/// `/etc` paths other phases put back (or that must never be copied between machines).
const ELSEWHERE: [&str; 20] = [
    "/etc/os-release",
    "/etc/localtime",
    "/etc/timezone",
    "/etc/fstab",
    "/etc/passwd",
    "/etc/group",
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/hostname",
    "/etc/machine-id",
    "/etc/netplan",
    "/etc/apt",
    "/etc/cron.d",
    "/etc/crontab",
    "/etc/anacrontab",
    "/etc/systemd/system",
    "/etc/moi",
    "/etc/docker",
    "/etc/resolv.conf",
    "/etc/mtab",
];

pub(super) fn config(run: &mut Run<'_>) {
    // Everything under /etc the snapshot kept is there because it is the server's own: files the owner
    // wrote, package configuration they changed, what units and scripts read. All of it comes back,
    // except what has a phase of its own.
    let etc = run.files.join("etc");
    let mut names = run.host.read_dir(&etc).unwrap_or_default();
    names.sort();
    let paths: Vec<String> = names.into_iter().map(|n| format!("/etc/{n}")).collect();
    let mut restored = 0;
    for path in paths {
        if ELSEWHERE.contains(&path.as_str()) {
            continue;
        }
        // a directory that is only partly someone else's: take what is not
        if ELSEWHERE.iter().any(|e| e.starts_with(&format!("{path}/"))) {
            for child in run.host.read_dir(&run.files.join(path.trim_start_matches('/'))).unwrap_or_default() {
                let child = format!("{path}/{child}");
                if !ELSEWHERE.contains(&child.as_str()) {
                    restored += run.restore_path(&child, false);
                }
            }
            continue;
        }
        restored += run.restore_path(&path, false);
    }
    run.info(&format!(
        "{restored} files under /etc{}",
        if run.dry() { " would be put back" } else { " put back (what they replaced is next to them as *.pre-moi)" }
    ));

    let old_host = run.snapshot_file("meta/hostname").trim().split('.').next().unwrap_or_default().to_string();
    if !old_host.is_empty()
        && old_host != crate::host::short_hostname(run.host.as_ref())
        && run.ask(
            &format!("Set the hostname to '{old_host}' (recommended: names in configs and monitors keep matching)?"),
            true,
        )
    {
        run.act(&["hostnamectl", "set-hostname", &old_host]);
    }
    let zone = run.snapshot_file("meta/timezone").trim().to_string();
    if !zone.is_empty() {
        run.act(&["timedatectl", "set-timezone", &zone]);
    }
    run.act(&["sysctl", "--system"]);
    if !run.dry() && run.have("visudo") && run.look(&["visudo", "-c"]).is_some_and(|o| o.status != 0) {
        run.warn("the sudoers files do not validate — fix /etc/sudoers.d (the previous files are *.pre-moi) before logging out");
    }
    // sshd: only a configuration that parses is loaded, and only when asked
    if run.have("sshd") && !run.dry() {
        if run.look(&["sshd", "-t"]).is_some_and(|o| o.status == 0) {
            if run.ask("Reload sshd with the restored configuration? (your key must be in ~/.ssh/authorized_keys — the home phase put it back)", true) && !run.act(&["systemctl", "reload", "ssh"]) {
                run.act(&["systemctl", "reload", "sshd"]);
            }
        } else {
            run.warn("the restored sshd configuration does not pass `sshd -t`: not reloaded. The previous files are *.pre-moi");
        }
    } else if run.dry() {
        run.say("    [dry-run] sshd -t, then (after asking) systemctl reload ssh");
    }
    // the network is shown, never applied: interface names differ between machines
    if run.captured("/etc/netplan") {
        let kept = "/var/lib/moi/restore/netplan-old";
        if !run.dry() {
            let source = run.files.join("etc/netplan");
            let _ = run.host.create_dir_all(Path::new(kept), 0o700);
            for name in run.host.read_dir(&source).unwrap_or_default() {
                if let Ok(content) = run.host.read(&source.join(&name)) {
                    let _ = run.host.write(&Path::new(kept).join(&name), &content, 0o600);
                }
            }
        }
        let old: Vec<String> = run
            .snapshot_lines("network/links.txt")
            .iter()
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect();
        run.warn("the network configuration is NOT applied (interface names differ between machines)");
        run.info(&format!("old interfaces: {}", old.join(" ")));
        run.info(&format!("the old netplan files are in {kept}: adapt them (static address, …), then `netplan try`"));
    }
}

pub(super) fn scripts(run: &mut Run<'_>) {
    let mut paths: Vec<String> = run.old.as_ref().map(|l| l.config.capture.script_dirs.clone()).unwrap_or_default();
    paths.extend(run.snapshot_lines("scripts/referenced.txt"));
    paths.extend(run.old.as_ref().map(|l| l.config.capture.extra.clone()).unwrap_or_default());
    // a script inside a directory that comes back whole is not named again
    let mut done: Vec<String> = Vec::new();
    for path in paths {
        if done.iter().any(|d| crate::backup::under(&path, d)) {
            continue;
        }
        run.restore_path(&path, false);
        done.push(path);
    }
}

pub(super) fn systemd(run: &mut Run<'_>) {
    // the unit moi installs replaces whatever the legacy setup had for it; it has its own phase
    let mut custom: Vec<String> = Vec::new();
    for file in run.snapshot_lines("systemd/custom-units.txt") {
        if file.ends_with("/moi.service") {
            continue;
        }
        run.restore_path(&file, false);
        if !file.ends_with(".conf") {
            custom.push(file.rsplit('/').next().unwrap_or(&file).to_string());
        }
    }
    run.act(&["systemctl", "daemon-reload"]);
    let enabled = run.snapshot_lines("systemd/enabled-units.txt");
    for unit in &enabled {
        if unit.contains("@.")
            || unit.starts_with("snap")
            || unit.starts_with("cloud-")
            || unit.starts_with("getty")
            || unit == "moi.service"
        {
            continue;
        }
        // only units that exist here (their package may not be installed) and are not enabled yet
        if !run.dry() {
            let known = run
                .look(&["systemctl", "list-unit-files", unit, "--no-legend"])
                .is_some_and(|o| !o.stdout_str().trim().is_empty());
            let on = run.look(&["systemctl", "is-enabled", "--quiet", unit]).is_some_and(|o| o.status == 0);
            if !known || on {
                continue;
            }
        }
        if !run.act(&["systemctl", "enable", unit]) {
            run.warn(&format!("could not enable {unit}"));
        }
    }
    let start: Vec<&String> = custom.iter().filter(|u| enabled.contains(u)).collect();
    if !start.is_empty() {
        run.say(&format!("  custom units: {}", custom.join(" ")));
        if run.ask("Start the enabled custom services and timers now?", true) {
            for unit in start {
                if !run.act(&["systemctl", "restart", unit]) {
                    run.warn(&format!("{unit} failed to start"));
                }
            }
        }
    }
}

pub(super) fn cron(run: &mut Run<'_>) {
    // two machines backing up into one repository is how repositories get damaged: say so first
    let mut jobs = String::new();
    for name in run.host.read_dir(&run.snap.join("cron")).unwrap_or_default().iter().filter(|n| n.ends_with(".crontab"))
    {
        jobs.push_str(&run.snapshot_file(&format!("cron/{name}")));
    }
    for file in ["etc/crontab"].iter().map(|f| run.files.join(f)).chain(
        run.host
            .read_dir(&run.files.join("etc/cron.d"))
            .unwrap_or_default()
            .into_iter()
            .map(|n| run.files.join("etc/cron.d").join(n)),
    ) {
        jobs.push_str(&run.host.read_to_string(&file).unwrap_or_default());
    }
    let backups: Vec<&str> = jobs
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter(|l| ["backup", "restic", "rclone", "borg", "kopia"].iter().any(|w| l.to_lowercase().contains(w)))
        .collect();
    if !backups.is_empty() {
        run.warn("these backup jobs will start running on THIS machine:");
        for job in &backups {
            run.say(&format!("      {job}"));
        }
        run.info("if the OLD server still runs its backup into the same repository, switch one of them off first");
        if !run.ask("Go on restoring cron?", true) {
            return;
        }
    }
    let users = run.ids("/etc/passwd");
    for name in run.host.read_dir(&run.snap.join("cron")).unwrap_or_default() {
        let Some(user) = name.strip_prefix("user-").and_then(|n| n.strip_suffix(".crontab")) else { continue };
        if !users.contains_key(user) && !run.dry() {
            run.warn(&format!("user {user} does not exist here: their crontab is skipped"));
            continue;
        }
        let file = run.snap.join("cron").join(&name);
        let file = file.to_string_lossy().into_owned();
        // `crontab` validates the file as it installs it
        let shown = Cmd::new("crontab").args(["-u", user, file.as_str()]);
        if run.act_cmd(shown) {
            run.info(&format!("crontab of {user}"));
        } else {
            run.warn(&format!("could not install the crontab of {user}"));
        }
    }
    for path in [
        "/etc/crontab",
        "/etc/anacrontab",
        "/etc/cron.d",
        "/etc/cron.hourly",
        "/etc/cron.daily",
        "/etc/cron.weekly",
        "/etc/cron.monthly",
    ] {
        run.restore_path(path, false);
    }
}
