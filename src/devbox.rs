//! devbox (docs/spec/15-devbox.md): moi embeds devbox's library `devbox-core` and offers its host
//! commands as `moi devbox …`, run through moi's own [`Host`], firewall and records — and observes
//! devbox like any other workload (the submodules).
//!
//! Everything devbox prints here is rendered by `devbox_core::render`, so `moi devbox` says what
//! `devbox` says, byte for byte. What moi adds is around it: the system access, the prompts, the
//! firewall rule in moi's own rules file, and the records afterwards.

pub mod records;

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use devbox_core::firewall::{self as dfw, Change, Manager, Rule};
use devbox_core::render;
use devbox_core::setup::{
    self, Confirm, DownOptions, PasswordKind, PasswordOptions, Paths, Plan, SecretString, StatusOptions, UpOptions, Yes,
};
use devbox_core::system::{self as dsys, System};

use crate::config::Config;
use crate::error::Error;
use crate::host::{self, Cmd, Host};
use crate::state::StateDir;

/// Where devbox keeps its host configuration.
pub const CONFIG: &str = "/etc/devbox/devbox.toml";

/// Whether the devbox integration is on: `[devbox] enabled`, or by itself where devbox is set up.
pub fn enabled(host: &dyn Host, config: &Config) -> bool {
    config.devbox.enabled.unwrap_or_else(|| host.exists(Path::new(CONFIG)))
}

/// devbox's host configuration, when the integration is on and the file reads.
pub fn host_config(host: &dyn Host, config: &Config) -> Option<toml::Table> {
    if !enabled(host, config) {
        return None;
    }
    host.read_to_string(Path::new(CONFIG)).ok()?.parse().ok()
}

/// Where devbox's gateway says how it is: asleep counts as healthy there.
pub fn health_url(host: &dyn Host, config: &Config) -> Option<String> {
    let code = host_config(host, config)?.get("hosts")?.get("code")?.as_str()?.to_string();
    Some(format!("https://{code}/__devbox/health"))
}

// ------------------------------------------------------------------------------------------------ the adapter

/// devbox's view of the machine a moi host stands for: `devbox_core::system::System`, method for
/// method the moi method of the same name (the table in devbox's spec 12). Only the types differ,
/// and a command with a working directory becomes `env -C <dir> …`, because moi's commands have none.
#[derive(Debug)]
pub struct DevboxHost<'a>(pub &'a dyn Host);

#[derive(Debug)]
struct Connection(Box<dyn host::Conn>);

impl Read for Connection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for Connection {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl dsys::Conn for Connection {
    fn set_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_timeout(timeout)
    }
}

impl System for DevboxHost<'_> {
    fn run(&self, command: &dsys::Command) -> io::Result<dsys::Output> {
        let mut cmd = match &command.cwd {
            Some(dir) => Cmd::new("env").arg("-C").arg(dir.to_string_lossy().into_owned()).arg(command.program.clone()),
            None => Cmd::new(command.program.clone()),
        };
        cmd = cmd.args(command.args.iter().cloned());
        for (name, value) in &command.env {
            cmd = cmd.env(name.clone(), value.clone());
        }
        if let Some(input) = &command.stdin {
            cmd = cmd.stdin(input.clone());
        }
        if let Some(limit) = command.timeout {
            cmd = cmd.timeout(limit);
        }
        let output = self.0.run(&cmd)?;
        Ok(dsys::Output {
            code: Some(output.status),
            timed_out: output.timed_out,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.0.read(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        self.0.read_dir(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.0.exists(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<dsys::Metadata> {
        let meta = self.0.metadata(path)?;
        Ok(dsys::Metadata {
            kind: match meta.kind {
                host::FileKind::File => dsys::FileKind::File,
                host::FileKind::Dir => dsys::FileKind::Dir,
                host::FileKind::Symlink => dsys::FileKind::Symlink,
                host::FileKind::Other => dsys::FileKind::Other,
            },
            size: meta.size,
            mode: meta.mode,
            uid: meta.uid,
            gid: meta.gid,
            mtime: meta.mtime,
        })
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.0.read_link(path)
    }

    fn write(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        // moi's write does not create directories; devbox's does
        if let Some(dir) = path.parent() {
            self.0.create_dir_all(dir, 0o755)?;
        }
        self.0.write(path, data, mode)
    }

    fn append(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            self.0.create_dir_all(dir, 0o755)?;
        }
        self.0.append(path, data, mode)
    }

    fn create_dir_all(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.0.create_dir_all(path, mode)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        // devbox's contract: a file that is not there is removed
        match self.0.remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    fn symlink(&self, target: &Path, path: &Path) -> io::Result<()> {
        self.0.symlink(target, path)
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.0.set_mode(path, mode)
    }

    fn set_owner(&self, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
        self.0.set_owner(path, uid, gid)
    }

    fn connect_unix(&self, path: &Path) -> io::Result<Box<dyn dsys::Conn>> {
        Ok(Box::new(Connection(self.0.connect_unix(path)?)))
    }

    fn env(&self, name: &str) -> Option<String> {
        self.0.env(name)
    }

    fn now(&self) -> SystemTime {
        self.0.now()
    }

    fn monotonic(&self) -> Duration {
        self.0.monotonic()
    }

    fn sleep(&self, duration: Duration) {
        self.0.sleep(duration);
    }

    fn random_bytes(&self, count: usize) -> io::Result<Vec<u8>> {
        self.0.random_bytes(count)
    }
}

// ------------------------------------------------------------------------------------------------ the firewall

const BLOCK_BEGIN: &str = "# devbox: begin";
const BLOCK_END: &str = "# devbox: end";

/// The rule devbox needs, as lines of moi's rules file.
fn block(rule: &Rule) -> String {
    let ports: Vec<String> = rule.ports.iter().map(u16::to_string).collect();
    format!(
        "{BLOCK_BEGIN}\n-A DOCKER-USER -s {} -d {}/32 -p tcp -m multiport --dports {} -m comment --comment \"{}\" -j ACCEPT\n{BLOCK_END}\n",
        rule.source,
        rule.address,
        ports.join(","),
        dfw::COMMENT
    )
}

/// A rules file with devbox's block in it (`Some`) or without it (`None`): replaced in place where
/// it is, otherwise put in front of the first `DOCKER-USER` rule, where it is reached before
/// whatever ends the chain. A rule a standalone `devbox` put there without the markers is taken
/// into the block.
pub fn rules_with(existing: &str, rule: Option<&Rule>) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut at: Option<usize> = None;
    let mut inside = false;
    for line in existing.lines() {
        if line.trim() == BLOCK_BEGIN {
            inside = true;
            at.get_or_insert(kept.len());
        } else if line.trim() == BLOCK_END {
            inside = false;
        } else if inside {
        } else if line.contains(dfw::COMMENT) {
            at.get_or_insert(kept.len());
        } else {
            kept.push(line);
        }
    }
    let Some(rule) = rule else {
        return if kept.is_empty() { String::new() } else { kept.join("\n") + "\n" };
    };
    let at = at.or_else(|| kept.iter().position(|l| l.starts_with("-A DOCKER-USER "))).unwrap_or(kept.len());
    let mut out: Vec<String> = kept[..at].iter().map(ToString::to_string).collect();
    out.extend(block(rule).lines().map(str::to_string));
    out.extend(kept[at..].iter().map(ToString::to_string));
    out.join("\n") + "\n"
}

/// moi's firewall manager for devbox: the rule lives in moi's own rules file as a marked block and
/// is applied the way `moi firewall apply` applies everything — saved rules first, and over SSH the
/// rollback timer and the typed confirmation.
pub struct MoiFirewall {
    pub host: Arc<dyn Host>,
    pub config: Config,
    pub state: StateDir,
    /// False during a rebuild: the block is put into the rules file, and the firewall phase that
    /// follows applies the whole file once.
    pub apply: bool,
}

impl std::fmt::Debug for MoiFirewall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MoiFirewall({})", self.config.firewall.rules_v4)
    }
}

impl MoiFirewall {
    fn put(&self, rule: Option<&Rule>, summary: String, apply: bool) -> Result<Change, dfw::Error> {
        let file = PathBuf::from(&self.config.firewall.rules_v4);
        let io = |source: io::Error| dfw::Error::Io { path: file.clone(), source };
        let existing = self.host.read_to_string(&file).unwrap_or_default();
        let wanted = rules_with(&existing, rule);
        let in_force = || {
            self.state
                .open_db(self.host.as_ref())
                .and_then(|db| {
                    crate::firewall::state(self.host.as_ref(), &self.config, &db, crate::firewall::Family::V4)
                })
                .is_ok_and(|state| state == crate::firewall::State::InSync)
        };
        if existing == wanted && (!self.apply || in_force()) {
            return Ok(Change { changed: false, summary, restore: Vec::new() });
        }
        if apply && !self.apply {
            if let Some(dir) = file.parent() {
                self.host.create_dir_all(dir, 0o755).map_err(io)?;
            }
            self.host.write(&file, wanted.as_bytes(), 0o644).map_err(io)?;
        } else if apply {
            if existing != wanted {
                if let Some(dir) = file.parent() {
                    self.host.create_dir_all(dir, 0o755).map_err(io)?;
                }
                self.host.write(&file, wanted.as_bytes(), 0o644).map_err(io)?;
            }
            let failed = |why: String| dfw::Error::Command { line: "moi firewall apply".to_string(), why };
            let db = self.state.open_db(self.host.as_ref()).map_err(|e| failed(e.to_string()))?;
            if let Err(e) = crate::firewall::apply_guarded(&self.host, &self.config, &self.state, &db) {
                // not in force: the file goes back to what it was, so that the two agree again
                let _ = self.host.write(&file, existing.as_bytes(), 0o644);
                return Err(failed(e.to_string()));
            }
        }
        // nothing to restore from: `down` calls `remove`
        Ok(Change { changed: true, summary, restore: Vec::new() })
    }
}

impl Manager for MoiFirewall {
    fn describe(&self) -> &str {
        "moi"
    }

    fn check(&self, _system: &dyn System, rule: &Rule) -> Result<Change, dfw::Error> {
        self.put(Some(rule), self.summary(rule), false)
    }

    fn ensure(&self, _system: &dyn System, rule: &Rule) -> Result<Change, dfw::Error> {
        self.put(Some(rule), self.summary(rule), true)
    }

    fn remove(&self, _system: &dyn System, rule: &Rule) -> Result<Change, dfw::Error> {
        self.put(None, format!("no rule for {} in {}", rule.address, self.config.firewall.rules_v4), true)
    }
}

impl MoiFirewall {
    fn summary(&self, rule: &Rule) -> String {
        let ports: Vec<String> = rule.ports.iter().map(u16::to_string).collect();
        format!(
            "containers → {} tcp {} in {} (moi's rules file, applied by moi with its rollback)",
            rule.address,
            ports.join(","),
            self.config.firewall.rules_v4
        )
    }
}

// ------------------------------------------------------------------------------------------------ the commands

/// What `moi devbox up` takes: devbox's own flags.
#[derive(Debug, Clone, Default)]
pub struct UpArgs {
    pub options: UpOptions,
    pub wake_password_file: Option<PathBuf>,
    pub app_password_file: Option<PathBuf>,
    pub dry_run: bool,
    pub yes: bool,
}

/// A command's text and exit code (0, or 1 for an unhealthy `doctor`), and whether it changed
/// something that the records should follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub code: u8,
    pub changed: bool,
}

/// What the commands need of moi.
#[derive(Debug)]
pub struct Cx<'a> {
    pub host: &'a Arc<dyn Host>,
    pub config: &'a Config,
    pub state: &'a StateDir,
}

impl Cx<'_> {
    fn manager(&self) -> Result<Arc<dyn Manager>, Error> {
        if !self.config.firewall.manage {
            return Err(Error::Usage(
                "moi does not manage this server's firewall (firewall.manage is off), so it cannot place devbox's rule.\nSwitch it on in moi.toml ([firewall] manage = true, with the rules files), or let devbox use the firewall that is there: `devbox up --firewall <host-firewall|ufw|nftables|none>`".to_string(),
            ));
        }
        Ok(Arc::new(MoiFirewall {
            host: Arc::clone(self.host),
            config: self.config.clone(),
            state: self.state.clone(),
            apply: true,
        }))
    }

    fn ask(&self, question: &str) -> Option<String> {
        self.host.ask(question, Duration::from_secs(600)).ok().flatten()
    }

    /// A password from a file (its first line), or asked for twice.
    fn password(&self, file: Option<&PathBuf>, what: &str, may_ask: bool) -> Result<Option<SecretString>, String> {
        if let Some(file) = file {
            let text = self.host.read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let line = text.lines().next().unwrap_or_default().to_string();
            if line.is_empty() {
                return Err(format!("{} is empty", file.display()));
            }
            return Ok(Some(SecretString::new(line)));
        }
        if !may_ask {
            return Ok(None);
        }
        // moi's terminal cannot hide what is typed: say so, and say how to avoid it
        let first = self
            .ask(&format!("{what} (visible as you type; a --…-password-file avoids that): "))
            .ok_or_else(|| format!("{what} is needed and nobody is there to ask: pass it in a file"))?;
        if first.is_empty() {
            return Err(format!("{what} must not be empty"));
        }
        if self.ask(&format!("{what}, again: ")).as_deref() != Some(first.as_str()) {
            return Err("the two do not match".to_string());
        }
        Ok(Some(SecretString::new(first)))
    }
}

/// The steps as they are applied.
struct Applying<'a> {
    out: &'a mut dyn Write,
    started: bool,
}

impl Confirm for Applying<'_> {
    fn confirm(&mut self, _: &Plan) -> bool {
        true
    }

    fn progress(&mut self, step: &str) {
        if !self.started {
            let _ = writeln!(self.out);
            self.started = true;
        }
        let _ = writeln!(self.out, "{step}");
    }
}

fn failed(command: &str, why: impl std::fmt::Display) -> Error {
    Error::Usage(format!("devbox {command}: {why}"))
}

/// `moi devbox up`: devbox's plan, a question, and its application — with moi's firewall manager.
pub fn up(cx: &Cx<'_>, args: &UpArgs, out: &mut dyn Write) -> Result<Done, Error> {
    let system = DevboxHost(cx.host.as_ref());
    let mut options = args.options.clone();
    options.paths = Paths::from_env(&system);
    options.firewall_manager = Some(cx.manager()?);
    options.firewall = None;
    options.dry_run = true;
    options.wake_password =
        cx.password(args.wake_password_file.as_ref(), "the wake password", false).map_err(|why| failed("up", why))?;
    options.app_password =
        cx.password(args.app_password_file.as_ref(), "the app password", false).map_err(|why| failed("up", why))?;
    // first the plan, which asks for nothing and changes nothing
    let planned = setup::up(&system, &options, &mut Yes).map_err(|e| Error::Usage(e.to_string()))?;
    let _ = write!(out, "{}", render::human(&planned));
    if args.dry_run {
        return Ok(Done { code: 0, changed: false });
    }
    let (needs_wake, needs_app) = (planned.plan.needs_wake_password, planned.plan.needs_app_password);
    let changes = planned.changes > 0 || needs_wake || needs_app;
    if changes && !args.yes && !matches!(cx.ask("Apply? [y/N] ").unwrap_or_default().trim(), "y" | "Y" | "yes") {
        let _ = writeln!(out, "Nothing was changed.");
        return Ok(Done { code: 0, changed: false });
    }
    // `--yes` agrees to the plan; it never makes up a password
    let asked = |needed: bool, what: &str| if needed { cx.password(None, what, !args.yes) } else { Ok(None) };
    let wake = asked(needs_wake, "The password that wakes the VM").map_err(|why| failed("up", why))?;
    let app = asked(needs_app, "The password of the agents app").map_err(|why| failed("up", why))?;
    options.wake_password = options.wake_password.or(wake);
    options.app_password = options.app_password.or(app);
    options.dry_run = false;
    let mut applying = Applying { out, started: false };
    let applied = setup::up(&system, &options, &mut applying);
    let started = applying.started;
    match applied {
        Ok(report) => {
            let _ = write!(out, "{}", render::outcome(&report));
            Ok(Done { code: 0, changed: report.changes > 0 })
        }
        Err(e) if started => Err(Error::Usage(format!("devbox up stopped: {e}"))),
        Err(e) => Err(Error::Usage(e.to_string())),
    }
}

/// `moi devbox down`: everything `up` changed on the host, back as it was; the rule out of moi's file.
pub fn down(cx: &Cx<'_>, destroy: bool, dry_run: bool, yes: bool, out: &mut dyn Write) -> Result<Done, Error> {
    let system = DevboxHost(cx.host.as_ref());
    let question = "Undo what `devbox up` changed on this host (routing, firewall, config, Incus settings)? [y/N] ";
    if !yes && !dry_run && !matches!(cx.ask(question).unwrap_or_default().trim(), "y" | "Y" | "yes") {
        let _ = writeln!(out, "Nothing was changed.");
        return Ok(Done { code: 0, changed: false });
    }
    let options =
        DownOptions { paths: Paths::from_env(&system), destroy, dry_run, firewall_manager: Some(cx.manager()?) };
    let report = setup::down(&system, &options).map_err(|e| Error::Usage(format!("devbox down stopped: {e}")))?;
    let _ = write!(out, "{}", render::human(&report));
    Ok(Done { code: 0, changed: !dry_run })
}

fn shown(report: &impl render::Report, json: bool) -> String {
    if json { render::json(report) } else { render::human(report) }
}

pub fn status(host: &dyn Host, json: bool, out: &mut dyn Write) -> Result<Done, Error> {
    let system = DevboxHost(host);
    let report =
        setup::status(&system, &StatusOptions { paths: Paths::from_env(&system) }).map_err(|e| failed("status", e))?;
    let _ = write!(out, "{}", shown(&report, json));
    Ok(Done { code: 0, changed: false })
}

pub fn doctor(host: &dyn Host, json: bool, out: &mut dyn Write) -> Result<Done, Error> {
    let system = DevboxHost(host);
    let report = setup::doctor(&system, &StatusOptions { paths: Paths::from_env(&system) });
    let _ = write!(out, "{}", shown(&report, json));
    Ok(Done { code: u8::from(!report.ok()), changed: false })
}

pub fn connect(host: &dyn Host, out: &mut dyn Write) -> Result<Done, Error> {
    let system = DevboxHost(host);
    let report = setup::connect(&system, &StatusOptions { paths: Paths::from_env(&system) })
        .map_err(|e| failed("connect", e))?;
    let _ = write!(out, "{}", render::human(&report));
    Ok(Done { code: 0, changed: false })
}

/// `moi devbox password wake|app`.
pub fn password(cx: &Cx<'_>, app: bool, file: Option<&PathBuf>, out: &mut dyn Write) -> Result<Done, Error> {
    let system = DevboxHost(cx.host.as_ref());
    let (kind, what) = if app {
        (PasswordKind::App, "The new password of the agents app")
    } else {
        (PasswordKind::Wake, "The new password that wakes the VM")
    };
    let new =
        cx.password(file, what, true).map_err(|why| failed("password", why))?.unwrap_or_else(|| SecretString::new(""));
    let report = setup::password(&system, &PasswordOptions { paths: Paths::from_env(&system), kind, new })
        .map_err(|e| failed("password", e))?;
    let _ = write!(out, "{}", render::human(&report));
    Ok(Done { code: 0, changed: true })
}

/// The devbox status as data, for the parts of moi that describe the server. `None` when devbox
/// is not set up or cannot be asked.
pub fn status_value(host: &dyn Host) -> Option<serde_json::Value> {
    let system = DevboxHost(host);
    setup::status(&system, &StatusOptions { paths: Paths::from_env(&system) }).ok().map(|report| report.0)
}

/// What a rebuild does (15 § Restore): `up` from the restored `/etc/devbox`, with no question asked
/// — the configuration says what the VM is, the secrets hold the password hashes. `adopt` when the
/// instance is there already. Returns devbox's own account of it, and its health afterwards.
pub fn rebuild(
    host: &Arc<dyn Host>,
    config: &Config,
    state: &StateDir,
    adopt: bool,
    dry_run: bool,
) -> Result<(String, Option<String>), String> {
    let system = DevboxHost(host.as_ref());
    let manager: Option<Arc<dyn Manager>> = config.firewall.manage.then(|| {
        Arc::new(MoiFirewall { host: Arc::clone(host), config: config.clone(), state: state.clone(), apply: false })
            as Arc<dyn Manager>
    });
    let options = UpOptions {
        paths: Paths::from_env(&system),
        adopt,
        firewall_manager: manager,
        dry_run,
        ..UpOptions::default()
    };
    let report = setup::up(&system, &options, &mut Yes).map_err(|e| e.to_string())?;
    let text = if dry_run { render::human(&report) } else { render::outcome(&report) };
    let health = if dry_run {
        None
    } else {
        status_value(host.as_ref()).and_then(|s| s["gateway"]["health"].as_str().map(str::to_string))
    };
    Ok((text, health))
}
