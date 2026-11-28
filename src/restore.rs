//! Rebuilding a server on new hardware from a state bundle and the data backup, and proving
//! afterwards — and after every boot — that everything that should run does.
//!
//! Spec: docs/spec/09-restore-verify.md. The phases are a port of the prototype's `restore_*`
//! functions (serverstate, the tool moi grew out of) and keep its safety rules: every phase asks first,
//! every file that is overwritten is kept as `*.pre-moi`, an error is a warning and the next thing is
//! tried, a disk is formatted only for someone who types `FORMAT /dev/…`, and a dry run prints every
//! action without doing any.

mod apps;
mod storage;
mod system;
pub mod verify;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Loaded, Paths};
use crate::error::Error;
use crate::host::{self, Cmd, CmdOutput, FileKind, Host};

/// The phases, in order, with what each does.
pub const PHASES: [(&str, &str); 20] = [
    ("preflight", "compare the old machine with this one; install base tools"),
    ("packages", "apt repositories and keys, manually installed packages, holds, snaps, pipx, npm"),
    ("users", "groups and users with the same ids, password hashes, group memberships"),
    ("home", "home directories as captured: dotfiles, ~/.ssh, tool configuration"),
    ("storage", "map the old mounts to this machine's disks; fstab"),
    ("dirs", "recreate the directory trees of the data roots with owners and modes"),
    ("docker-engine", "install Docker and the compose plugin; /etc/docker and its drop-ins"),
    ("data", "restore the backed-up data from the off-site repository"),
    ("config", "/etc as it was: ssh, sysctl, sudoers and every other captured file; hostname, time zone"),
    ("ssh", "the tunnel connector for SSH from anywhere, its ingress and DNS confirmed; the public handshake"),
    ("scripts", "script directories and every script a unit or cron job refers to"),
    ("incus", "Incus with its storage pools, networks and profiles; instances as declared"),
    ("docker", "networks, databases first with their dumps, then every stack; swarm services"),
    ("moi", "moi's own configuration and history; the service"),
    ("systemd", "remaining custom units; units that were enabled"),
    ("cron", "crontabs and /etc/cron.*"),
    ("devbox", "the devbox VM rebuilt from the restored /etc/devbox, or adopted when its disk came along"),
    ("firewall", "the managed firewall chains, with the way back arranged first"),
    ("monitors", "Uptime Kuma: every monitor must be found again, none created"),
    ("verify", "check everything against the old server; the reboot that proves it"),
];

/// How to run.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dry_run: bool,
    pub only: Vec<String>,
    pub from: Option<String>,
    pub skip: Vec<String>,
    /// Do not ask before each phase. Never answers a disk-format or firewall question.
    pub yes: bool,
    /// A restic snapshot to restore the data from, instead of the latest one.
    pub snapshot: Option<String>,
}

/// One restore in progress: where the bundle is, how to act, where to say what happens.
pub struct Run<'a> {
    pub host: &'a Arc<dyn Host>,
    pub options: &'a Options,
    /// The old server's state: `snapshot/` (with `files/`), `volatile/`.
    pub snap: PathBuf,
    pub vol: PathBuf,
    pub files: PathBuf,
    /// The old server's `/etc/moi`.
    pub etc_moi: PathBuf,
    /// The old server's configuration, when the bundle's loads.
    pub old: Option<Loaded>,
    /// mode, owner, group of every captured file.
    manifest: BTreeMap<String, (u32, String, String)>,
    out: &'a mut dyn Write,
    pub warnings: usize,
}

impl std::fmt::Debug for Run<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Run({})", self.snap.display())
    }
}

pub const LOG: &str = "/var/lib/moi/restore/restore.log";

impl<'a> Run<'a> {
    /// Open a bundle directory as `moi pull` leaves it (`var/lib/moi/state`, `etc/moi`).
    pub fn open(
        host: &'a Arc<dyn Host>,
        bundle: &Path,
        options: &'a Options,
        out: &'a mut dyn Write,
    ) -> Result<Self, Error> {
        let state = bundle.join("var/lib/moi/state");
        let snap = state.join("snapshot");
        if !host.exists(&snap) {
            return Err(Error::Usage(format!(
                "{} does not look like a state bundle (no var/lib/moi/state/snapshot in it): give the directory `moi pull` printed",
                bundle.display()
            )));
        }
        let etc_moi = bundle.join("etc/moi");
        let mut manifest = BTreeMap::new();
        for line in host.read_to_string(&snap.join("files.manifest.tsv")).unwrap_or_default().lines() {
            let cells: Vec<&str> = line.split('\t').collect();
            if let [mode, owner, group, path, ..] = cells.as_slice() {
                manifest.insert(
                    (*path).to_string(),
                    (u32::from_str_radix(mode, 8).unwrap_or(0o644), (*owner).to_string(), (*group).to_string()),
                );
            }
        }
        // the old server's settings say where its data lives and how to get it back
        let old = crate::config::load(host.as_ref(), &Paths::new(etc_moi.join("moi.toml"), None)).ok();
        Ok(Self {
            host,
            options,
            vol: state.join("volatile"),
            files: snap.join("files"),
            snap,
            etc_moi,
            old,
            manifest,
            out,
            warnings: 0,
        })
    }

    pub fn dry(&self) -> bool {
        self.options.dry_run
    }

    /// Say something: to the terminal, and (when really running) to the restore log.
    pub fn say(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
        if !self.dry() {
            let _ = self.host.append(Path::new(LOG), format!("{line}\n").as_bytes(), 0o600);
        }
    }

    pub fn info(&mut self, message: &str) {
        self.say(&format!("    {message}"));
    }

    pub fn ok(&mut self, message: &str) {
        self.say(&format!("  ✓ {message}"));
    }

    /// Something went wrong that the restore goes on from.
    pub fn warn(&mut self, message: &str) {
        self.warnings += 1;
        self.say(&format!("WARN: {message}"));
    }

    /// A yes/no question. `--yes` takes the default-yes ones; without a terminal the default answers.
    pub fn ask(&mut self, question: &str, default: bool) -> bool {
        if self.options.yes || self.dry() {
            return default;
        }
        let hint = if default { "Y/n" } else { "y/N" };
        match self.host.ask(&format!("  {question} [{hint}] "), Duration::from_secs(3600)).ok().flatten() {
            Some(answer) if answer.trim().is_empty() => default,
            Some(answer) => answer.trim().starts_with(['y', 'Y']),
            None => default,
        }
    }

    /// A question only a person can answer (never taken by `--yes`). Empty when nobody is there.
    pub fn prompt(&mut self, question: &str) -> String {
        self.host
            .ask(&format!("  {question} "), Duration::from_secs(3600))
            .ok()
            .flatten()
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    /// Do something that changes the machine — or, in a dry run, say what would be done.
    pub fn act(&mut self, command: &[&str]) -> bool {
        self.act_cmd(Cmd::new(command[0]).args(command[1..].iter().copied()))
    }

    pub fn act_cmd(&mut self, cmd: Cmd) -> bool {
        if self.dry() {
            self.say(&format!("    [dry-run] {}", cmd.line()));
            return true;
        }
        let line = cmd.line();
        match self.host.run(&cmd.timeout(Duration::from_secs(3600))) {
            Ok(out) if out.status == 0 => true,
            Ok(out) => {
                let said = out.stderr_str();
                let _ = self.host.append(
                    Path::new(LOG),
                    format!("    `{line}` exited {}: {}\n", out.status, said.trim()).as_bytes(),
                    0o600,
                );
                false
            }
            Err(e) => {
                let _ =
                    self.host.append(Path::new(LOG), format!("    `{line}` could not run: {e}\n").as_bytes(), 0o600);
                false
            }
        }
    }

    /// Ask the machine something (never changes it, so it also runs in a dry run).
    pub fn look(&self, command: &[&str]) -> Option<CmdOutput> {
        self.host.run(&Cmd::new(command[0]).args(command[1..].iter().copied()).timeout(Duration::from_secs(60))).ok()
    }

    pub fn have(&self, tool: &str) -> bool {
        ["/usr/bin", "/usr/sbin", "/bin", "/sbin", "/usr/local/bin", "/snap/bin"]
            .iter()
            .any(|dir| self.host.exists(&Path::new(dir).join(tool)))
    }

    /// A file of the old server's snapshot, or nothing.
    pub fn snapshot_file(&self, relative: &str) -> String {
        self.host.read_to_string(&self.snap.join(relative)).unwrap_or_default()
    }

    pub fn snapshot_lines(&self, relative: &str) -> Vec<String> {
        self.snapshot_file(relative).lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
    }

    pub fn volatile_file(&self, relative: &str) -> String {
        self.host.read_to_string(&self.vol.join(relative)).unwrap_or_default()
    }

    /// Whether the bundle has a copy of this host path.
    pub fn captured(&self, path: &str) -> bool {
        self.host.exists(&self.files.join(path.trim_start_matches('/')))
    }

    /// Names → ids on this machine, for giving restored files their owners back.
    fn ids(&self, file: &str) -> BTreeMap<String, u32> {
        self.host
            .read_to_string(Path::new(file))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut cells = l.split(':');
                Some((cells.next()?.to_string(), cells.nth(1)?.parse().ok()?))
            })
            .collect()
    }

    /// Put a captured path (file or directory) back where it was. Files that exist and differ are kept
    /// as `<name>.pre-moi` first — unless `keep_existing`, which leaves them alone. Returns how many
    /// files were written.
    pub fn restore_path(&mut self, path: &str, keep_existing: bool) -> usize {
        let source = self.files.join(path.trim_start_matches('/'));
        if self.host.symlink_metadata(&source).is_err() {
            return 0;
        }
        // every file below it, in order
        let mut entries: Vec<(PathBuf, FileKind)> = Vec::new();
        let mut stack = vec![source.clone()];
        while let Some(at) = stack.pop() {
            let Ok(meta) = self.host.symlink_metadata(&at) else { continue };
            if meta.kind == FileKind::Dir {
                let mut names = self.host.read_dir(&at).unwrap_or_default();
                names.sort();
                stack.extend(names.into_iter().rev().map(|n| at.join(n)));
            } else {
                entries.push((at, meta.kind));
            }
        }
        if self.dry() {
            self.say(&format!(
                "    [dry-run] restore {path} ({} files{})",
                entries.len(),
                if keep_existing { ", keeping what is there" } else { "" }
            ));
            return entries.len();
        }
        let (users, groups) = (self.ids("/etc/passwd"), self.ids("/etc/group"));
        let mut written = 0;
        for (from, kind) in entries {
            let Ok(relative) = from.strip_prefix(&self.files) else { continue };
            let target = Path::new("/").join(relative);
            let exists = self.host.symlink_metadata(&target).is_ok();
            if exists && keep_existing {
                continue;
            }
            if let Some(parent) = target.parent() {
                let _ = self.host.create_dir_all(parent, 0o755);
            }
            let (mode, owner, group) = self.manifest.get(&target.to_string_lossy().into_owned()).cloned().unwrap_or((
                0o644,
                "root".into(),
                "root".into(),
            ));
            let done = if kind == FileKind::Symlink {
                self.host.read_link(&from).and_then(|link| self.host.symlink(&link, &target))
            } else {
                let Ok(content) = self.host.read(&from) else { continue };
                if exists && self.host.read(&target).is_ok_and(|now| now == content) {
                    continue;
                }
                // what was there stays, once, next to it
                let kept = PathBuf::from(format!("{}.pre-moi", target.display()));
                if exists
                    && !self.host.exists(&kept)
                    && let Ok(previous) = self.host.read(&target)
                {
                    let previous_mode = self.host.metadata(&target).map_or(0o600, |m| m.mode);
                    let _ = self.host.write(&kept, &previous, previous_mode);
                }
                self.host.write(&target, &content, mode)
            };
            match done {
                Ok(()) => {
                    let _ = self.host.set_owner(
                        &target,
                        users.get(&owner).copied().unwrap_or(0),
                        groups.get(&group).copied().unwrap_or(0),
                    );
                    written += 1;
                }
                Err(e) => self.warn(&format!("could not restore {}: {e}", target.display())),
            }
        }
        written
    }

    /// `apt-get install` without questions.
    pub fn apt_install(&mut self, packages: &[&str]) -> bool {
        let mut cmd = Cmd::new("apt-get").args(["install", "-y", "-q"]).env("DEBIAN_FRONTEND", "noninteractive");
        cmd = cmd.args(packages.iter().copied());
        self.act_cmd(cmd)
    }

    /// When the snapshot in the bundle was taken.
    pub fn captured_at(&self) -> String {
        self.volatile_file("captured-at").trim().to_string()
    }
}

/// Which phases a set of options selects, in order.
pub fn selected(options: &Options) -> Result<Vec<&'static str>, Error> {
    let known = |name: &String| PHASES.iter().any(|(phase, _)| phase == name);
    for name in options.only.iter().chain(&options.skip).chain(options.from.iter()) {
        if !known(name) {
            return Err(Error::Usage(format!("no such phase: {name} (`moi restore --list` shows them)")));
        }
    }
    let mut started = options.from.is_none();
    let mut out = Vec::new();
    for (phase, _) in PHASES {
        started |= options.from.as_deref() == Some(phase);
        let wanted = options.only.is_empty() || options.only.iter().any(|o| o == phase);
        if started && wanted && !options.skip.iter().any(|s| s == phase) {
            out.push(phase);
        }
    }
    Ok(out)
}

/// Rebuild this machine from a bundle. Returns the number of warnings.
pub fn restore(host: &Arc<dyn Host>, bundle: &Path, options: &Options, out: &mut dyn Write) -> Result<usize, Error> {
    let phases = selected(options)?;
    let mut run = Run::open(host, bundle, options, out)?;
    if !run.dry() {
        host.create_dir_all(Path::new(LOG).parent().unwrap_or(Path::new("/")), 0o700)
            .map_err(|e| Error::host("cannot create the restore log's directory", e))?;
    }
    let stamp = host::format_rfc3339(host.now());
    run.say(&format!(
        "moi restore{}  bundle: {}  ({stamp})",
        if options.dry_run { " (DRY RUN — nothing will change)" } else { "" },
        bundle.display()
    ));
    for phase in phases {
        let what = PHASES.iter().find(|(name, _)| *name == phase).map_or("", |(_, what)| what);
        run.say("");
        run.say(&format!("==> phase: {phase} — {what}"));
        // every phase asks first (the look around before anything changes does not)
        if !run.dry() && phase != "preflight" && !run.ask(&format!("Run phase '{phase}'?"), true) {
            run.info("skipped");
            continue;
        }
        match phase {
            "preflight" => system::preflight(&mut run),
            "packages" => system::packages(&mut run),
            "users" => system::users(&mut run),
            "home" => system::home(&mut run),
            "storage" => storage::storage(&mut run),
            "dirs" => system::dirs(&mut run),
            "docker-engine" => apps::docker_engine(&mut run),
            "data" => apps::data(&mut run),
            "config" => system::config(&mut run),
            "ssh" => apps::ssh(&mut run),
            "scripts" => system::scripts(&mut run),
            "incus" => apps::incus(&mut run),
            "docker" => apps::docker(&mut run),
            "moi" => apps::moi(&mut run),
            "systemd" => system::systemd(&mut run),
            "cron" => system::cron(&mut run),
            "devbox" => apps::devbox(&mut run),
            "firewall" => apps::firewall(&mut run),
            "monitors" => apps::monitors(&mut run),
            "verify" => apps::verify(&mut run),
            _ => {}
        }
    }
    run.say("");
    let warnings = run.warnings;
    let ending = if options.dry_run {
        "dry run finished: nothing was changed".to_string()
    } else {
        format!("restore finished with {warnings} warnings. Log: {LOG}")
    };
    run.say(&ending);
    Ok(warnings)
}
