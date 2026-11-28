//! Command-line interface: argument definitions and dispatch.
//!
//! `src/main.rs` only calls [`run`].
//!
//! Output contract: results on stdout (human text, or one JSON document with `--json` that always has
//! an `ok` field), diagnostics and log lines on stderr, exit codes as in `error`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use serde_json::json;

use crate::config::{Dur, Paths};
use crate::error::Error;
use crate::host::{Cmd, FixtureHost, Host, RealHost};
use crate::log::{Level, Logger};
use crate::redact::Masker;
use crate::state::StateDir;
use crate::state::history::{self, History};

/// moi — run a Linux server from one binary.
#[derive(Debug, Parser)]
#[command(name = "moi", version = crate::VERSION_LONG, about, long_about = None, propagate_version = true)]
pub struct Cli {
    /// Config file
    #[arg(long, global = true, env = "MOI_CONFIG", default_value = "/etc/moi/moi.toml")]
    pub config: PathBuf,

    /// Secrets file [default: secrets.toml next to the config file]
    #[arg(long, global = true, env = "MOI_SECRETS")]
    pub secrets: Option<PathBuf>,

    /// Machine-readable JSON output where supported
    #[arg(long, global = true)]
    pub json: bool,

    /// Only print warnings and errors
    #[arg(long, short, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Discover the server, write config, install the service, take the first snapshot
    Init(InitArgs),
    /// Capture the full server state into the versioned state dir
    Snapshot(SnapshotArgs),
    /// Show the change history of the server
    Log {
        /// Number of entries
        #[arg(short = 'n', default_value_t = 40)]
        count: usize,
    },
    /// Show what changed in a snapshot (default: the latest)
    Diff {
        /// Revision (git syntax, e.g. HEAD~3)
        #[arg(default_value = "HEAD")]
        rev: String,
        /// Show secret values instead of masking them (root only)
        #[arg(long)]
        show_secrets: bool,
    },
    /// Regenerate CONTEXT.md for AI assistants, or share it with an expiring link
    Context {
        #[command(subcommand)]
        action: Option<ContextCommand>,
    },
    /// Edit the owner's notes merged into CONTEXT.md
    Notes,
    /// Find what is not backed up, not restorable, lost on reboot, or on the wrong disk
    Audit,
    /// Run health checks
    Check(CheckArgs),
    /// Uptime Kuma monitors
    #[command(subcommand)]
    Monitors(MonitorsCommand),
    /// Back up databases and identity data locally and off-site
    Backup(BackupArgs),
    /// Upload the encrypted state bundle off-site
    Push {
        /// Push even if nothing changed
        #[arg(long)]
        force: bool,
    },
    /// Download the latest state bundle (on a new machine)
    Pull {
        /// restic repository URL (default: from config)
        #[arg(long)]
        repository: Option<String>,
    },
    /// Rebuild this machine from a state bundle and the data backup
    Restore(RestoreArgs),
    /// Check that everything that should run, runs (also after every boot)
    Verify,
    /// Print what you must keep outside the server to rebuild it
    RecoveryCard,
    /// Host firewall rules managed by moi
    #[command(subcommand)]
    Firewall(FirewallCommand),
    /// Move a server from legacy scripts and cron jobs to moi
    #[command(subcommand)]
    Migrate(MigrateCommand),
    /// Configuration
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Daemon status, last results and next scheduled jobs
    Status,
    /// Run the daemon (used by moi.service)
    Daemon,
    /// Remove moi and restore anything it replaced
    Uninstall {
        /// Also delete state, history and archives
        #[arg(long)]
        purge: bool,
    },
    /// SSH to this server from anywhere, through a tunnel
    #[command(subcommand)]
    Ssh(SshCommand),
    /// Set up and run a devbox VM (devbox's own commands, with moi's firewall and records)
    #[command(subcommand)]
    Devbox(DevboxCommand),
    /// Print a shell completion script (bash, zsh, fish)
    Completions { shell: clap_complete::Shell },
    /// Print the man page (roff)
    #[command(hide = true)]
    Man,
}

#[derive(Debug, Subcommand)]
pub enum DevboxCommand {
    /// Set up or adopt the VM, the gateway's config, routing and the firewall rule
    Up(Box<DevboxUpArgs>),
    /// Undo routing, firewall and config changes, restoring what was there
    Down {
        /// Keep the VM (the default; here so that the choice can be written out)
        #[arg(long, conflicts_with = "destroy")]
        keep_vm: bool,
        /// Also delete the VM, if `devbox up` created it
        #[arg(long)]
        destroy: bool,
        /// Show what would be undone and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Do not ask before undoing
        #[arg(long)]
        yes: bool,
    },
    /// Show the VM, gateway, routing, hub and health on one screen
    Status,
    /// Run every check and print the fix for each failure
    Doctor,
    /// Print the URLs, an SSH config snippet and how to install the phone app
    Connect,
    /// Rotate a password
    Password {
        /// Which password to rotate
        which: DevboxPassword,
        /// A file with the new password, instead of being asked for it
        #[arg(long, value_name = "FILE")]
        password_file: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DevboxPassword {
    /// The password that wakes the VM
    Wake,
    /// The agents app password
    App,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DevboxRouting {
    /// Write a Traefik dynamic configuration file
    TraefikFile,
    /// Route the hosts yourself
    None,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DevboxTokenSource {
    Gh,
    Env,
}

/// The flags of `devbox up` (the firewall is always moi's here, so `--firewall` is not among them).
#[derive(Debug, Args)]
pub struct DevboxUpArgs {
    /// The domain: the editor is at code.<domain>, the agents app at agents.<domain>
    #[arg(long)]
    pub domain: Option<String>,
    /// The editor's host, when it is not code.<domain>
    #[arg(long, value_name = "HOST")]
    pub code_host: Option<String>,
    /// The agents app's host, when it is not agents.<domain>
    #[arg(long, value_name = "HOST")]
    pub agents_host: Option<String>,
    /// The Incus instance [default: devbox]
    #[arg(long, value_name = "NAME")]
    pub instance: Option<String>,
    /// Use an instance that exists already; its disk and data are kept
    #[arg(long)]
    pub adopt: bool,
    /// vCPUs of a new VM [default: 3]
    #[arg(long)]
    pub cpu: Option<u32>,
    /// Memory of a new VM [default: 5GiB]
    #[arg(long)]
    pub memory: Option<String>,
    /// Disk of a new VM [default: 60GiB]
    #[arg(long)]
    pub disk: Option<String>,
    /// Image of a new VM [default: images:ubuntu/26.04/cloud]
    #[arg(long)]
    pub image: Option<String>,
    /// The user in the VM [default: the user who ran sudo]
    #[arg(long)]
    pub user: Option<String>,
    /// Your dotfiles repository, cloned into the VM and applied
    #[arg(long, value_name = "GIT URL")]
    pub dotfiles: Option<String>,
    /// Idle time before the VM sleeps [default: 15m]
    #[arg(long, value_name = "DURATION")]
    pub idle: Option<String>,
    /// Agents not to install (claude, codex, opencode)
    #[arg(long, value_name = "AGENTS", value_delimiter = ',')]
    pub no_agents: Vec<String>,
    /// How the hosts are routed to the gateway [default: traefik-file]
    #[arg(long)]
    pub routing: Option<DevboxRouting>,
    /// The Traefik dynamic configuration file to write
    #[arg(long, value_name = "PATH")]
    pub traefik_file: Option<PathBuf>,
    /// Give the VM this host's GitHub token: from gh, or from the environment
    #[arg(long, value_name = "SOURCE")]
    pub github_token_from: Option<DevboxTokenSource>,
    /// SSH public keys for a new VM's user [default: the keys you log in with]
    #[arg(long, value_name = "FILE")]
    pub ssh_keys: Option<PathBuf>,
    /// A file with the password that wakes the VM
    #[arg(long, value_name = "FILE")]
    pub wake_password_file: Option<PathBuf>,
    /// A file with the password of the agents app
    #[arg(long, value_name = "FILE")]
    pub app_password_file: Option<PathBuf>,
    /// A directory with devbox-hub and devbox built for the VM, instead of a release
    #[arg(long, value_name = "DIR")]
    pub binaries: Option<PathBuf>,
    /// The release to install into the VM [default: the latest]
    #[arg(long, value_name = "TAG")]
    pub release: Option<String>,
    /// Show what would change and change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Do not ask before applying (never supplies a password)
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Subcommand)]
pub enum SshCommand {
    /// Set up or adopt the tunnel path (idempotent)
    Up {
        /// The public host name that answers SSH (default: `[ssh] hostname`)
        #[arg(long)]
        hostname: Option<String>,
        /// Print the plan without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Don't ask before applying the plan
        #[arg(long)]
        yes: bool,
        /// Used by moi itself: apply the tunnel steps into this record (the unit behind the rollback timer)
        #[arg(long, hide = true, value_name = "DIR")]
        inner: Option<PathBuf>,
    },
    /// Connector, ingress, DNS, public handshake, keys
    Status,
    /// Print the one command to run on a computer
    Connect,
    /// Allow a computer's key to log in
    Authorize {
        /// The public key line, or a file that holds it
        key: String,
        /// A name for the key (its comment)
        #[arg(long)]
        name: Option<String>,
    },
    /// Keep a change made over the tunnel
    Confirm,
    /// Restore what `up` replaced
    Down {
        /// Print what would be restored without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Used by moi itself: undo this record (the rollback timer)
        #[arg(long, hide = true, value_name = "DIR")]
        rollback: Option<PathBuf>,
    },
}

/// The completion script for a shell, as packages install it.
pub fn completions(shell: clap_complete::Shell) -> String {
    use clap::CommandFactory;
    let mut out = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "moi", &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// `moi(1)`.
pub fn man_page() -> String {
    use clap::CommandFactory;
    let mut out = Vec::new();
    // the version line would change the page with every build
    let command = Cli::command().version(env!("CARGO_PKG_VERSION")).long_version(None::<&str>);
    let _ = clap_mangen::Man::new(command).render(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Do not prompt; write defaults and secret: references
    #[arg(long)]
    pub non_interactive: bool,
    /// Only capture state (no service, no checks, no backups)
    #[arg(long)]
    pub observe: bool,
}

#[derive(Debug, Args)]
pub struct SnapshotArgs {
    /// Push the state bundle afterwards (if changed)
    #[arg(long)]
    pub push: bool,
    /// Override the state directory
    #[arg(long)]
    pub state: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Only this check (resources, storage, services, or a custom check name)
    pub name: Option<String>,
    /// Push results to Uptime Kuma (manual runs never push otherwise)
    #[arg(long)]
    pub push: bool,
}

#[derive(Debug, Subcommand)]
pub enum ContextCommand {
    /// Upload CONTEXT.md to the private share bucket and print an expiring link
    Share {
        /// Link lifetime such as 30m or 2h (default from config, capped at context.share.max_ttl)
        #[arg(long)]
        ttl: Option<Dur>,
        /// Delete every shared copy now (links stop working immediately)
        #[arg(long, conflicts_with_all = ["ttl", "list"])]
        revoke: bool,
        /// List active shares and their expiry
        #[arg(long, conflicts_with = "ttl")]
        list: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum MonitorsCommand {
    /// Adopt existing monitors and create missing ones
    Sync {
        /// Print the plan only (works with read-only access to kuma.db)
        #[arg(long)]
        dry_run: bool,
        /// Apply drift (never changes push tokens)
        #[arg(long)]
        update: bool,
        /// Delete orphaned monitors that moi created
        #[arg(long)]
        prune: bool,
    },
    /// List monitors and which check owns each
    List,
}

#[derive(Debug, Args)]
pub struct BackupArgs {
    /// Show what would be backed up (paths, dumps, sizes, why) and run every guard, without writing
    #[arg(long)]
    pub dry_run: bool,
    /// Restore-test the latest backup into throwaway containers
    #[arg(long)]
    pub drill: bool,
}

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Extracted bundle directory (from `moi pull`)
    #[arg(long)]
    pub bundle: Option<PathBuf>,
    /// Print every action without changing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Only these phases (comma separated)
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,
    /// Start at this phase
    #[arg(long)]
    pub from: Option<String>,
    /// Skip these phases (comma separated)
    #[arg(long, value_delimiter = ',')]
    pub skip: Vec<String>,
    /// Don't ask before each phase (never skips disk-format or firewall confirmations)
    #[arg(long)]
    pub yes: bool,
    /// List the phases
    #[arg(long)]
    pub list: bool,
    /// Restore the data from this restic snapshot instead of the latest one
    #[arg(long, value_name = "ID")]
    pub snapshot: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum FirewallCommand {
    /// Apply the managed chains (auto-rollback when run over SSH)
    Apply,
    /// Show the difference between live and configured rules
    Diff,
    /// Show which chains moi manages and their state
    Status,
}

#[derive(Debug, Subcommand)]
pub enum MigrateCommand {
    /// Show what will be replaced by what (read-only)
    Plan,
    /// Run moi checks without pushing while legacy keeps running
    Shadow,
    /// Compare moi's shadow messages with what Kuma received from legacy scripts
    Compare,
    /// Hand items over from legacy to moi (same monitors, same tokens)
    Switch {
        /// Only these items (comma separated)
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Switch even if comparisons are not clean
        #[arg(long)]
        force: bool,
    },
    /// Undo switched items from the archive
    Rollback {
        /// Only these items (comma separated)
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
    },
    /// Remove archived legacy files from disk and finish the migration
    Finish {
        /// Don't wait for the 7-day stability window
        #[arg(long)]
        now: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Validate config and secrets, print the effective config (secrets masked)
    Check,
    /// Print the path of the config and secrets files
    Path,
}

/// Parse arguments and run the command. Returns the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let result = select_host().and_then(|host| dispatch(&cli, host, &mut out));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            report_error(&cli, &e, &mut out);
            ExitCode::from(e.exit_code())
        }
    }
}

/// The machine to act on: the real one, or — when `MOI_ROOT` names a fixture directory
/// (`tests/fixtures/hosts/<name>`) — a fake one that never runs a command or touches the real system.
/// Its writes go to `MOI_ROOT_OVERLAY` if set and are refused otherwise.
///
/// This and argument parsing are the only places that read the process environment directly;
/// everything after goes through the host.
fn select_host() -> Result<Arc<dyn Host>, Error> {
    let Some(root) = std::env::var_os("MOI_ROOT") else { return Ok(Arc::new(RealHost::new())) };
    let root = PathBuf::from(root);
    let ctx = |e| Error::host(format!("MOI_ROOT={}", root.display()), e);
    let mut host = FixtureHost::open(&root).map_err(ctx)?;
    if let Some(overlay) = std::env::var_os("MOI_ROOT_OVERLAY") {
        host = host.with_overlay(Path::new(&overlay)).map_err(ctx)?;
    }
    let passed = |name: &str| name.starts_with("MOI_") || name == "EDITOR" || name == "VISUAL";
    for (name, value) in std::env::vars().filter(|(name, _)| passed(name)) {
        host.set_env(&name, &value);
    }
    Ok(Arc::new(host))
}

/// Parse a command line (without the program name). For tests and embedding; `run` is the real entry.
pub fn parse(args: &[&str]) -> Result<Cli, String> {
    Cli::try_parse_from(std::iter::once("moi").chain(args.iter().copied())).map_err(|e| e.to_string())
}

/// Run one parsed command against a host, writing its result to `out`.
pub fn execute(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    dispatch(cli, host, out)
}

fn dispatch(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    match &cli.command {
        Command::Config(cmd) => config(cli, cmd, host, out),
        Command::Snapshot(args) => snapshot(cli, args, host, out),
        Command::Log { count } => log(cli, *count, host, out),
        Command::Diff { rev, show_secrets } => diff(cli, rev, *show_secrets, host, out),
        Command::Notes => notes(cli, host, out),
        Command::Audit => audit(cli, host, out),
        Command::Context { action: None } => context(cli, host, out),
        Command::Context { action: Some(ContextCommand::Share { ttl, revoke, list }) } => {
            context_share(cli, *ttl, *revoke, *list, host, out)
        }
        Command::Check(args) => check(cli, args, host, out),
        Command::Monitors(command) => monitors(cli, command, host, out),
        Command::Daemon => {
            let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
            crate::daemon::Daemon::new(host, loaded)?.run()
        }
        Command::Status => status(cli, host, out),
        Command::Firewall(command) => firewall(cli, command, host, out),
        Command::Init(args) => init(cli, args, host, out),
        Command::Uninstall { purge } => uninstall(cli, *purge, host, out),
        Command::Backup(args) => backup(cli, args, host, out),
        Command::Push { force } => push(cli, *force, host, out),
        Command::Pull { repository } => pull(cli, repository.as_deref(), host, out),
        Command::RecoveryCard => {
            let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
            let card = crate::bundle::recovery_card(host.as_ref(), &loaded.config, &cli.paths().config);
            // belt and braces: whatever was assembled goes through the masker that knows every secret
            let card = loaded.masker.mask(&card);
            if cli.json {
                print_json(out, &json!({ "ok": true, "card": card }));
            } else {
                let _ = write!(out, "{card}");
            }
            Ok(())
        }
        Command::Verify => verify(cli, host, out),
        Command::Ssh(command) => ssh(cli, command, host, out),
        Command::Devbox(command) => devbox(cli, command, host, out),
        Command::Completions { shell } => {
            let _ = write!(out, "{}", completions(*shell));
            Ok(())
        }
        Command::Man => {
            let _ = write!(out, "{}", man_page());
            Ok(())
        }
        Command::Restore(args) => restore(cli, args, host, out),
        Command::Migrate(MigrateCommand::Plan) => migrate_plan(cli, host, out),
        Command::Migrate(MigrateCommand::Shadow) => migrate_shadow(cli, host, out),
        Command::Migrate(MigrateCommand::Compare) => migrate_compare(cli, host, out),
        Command::Migrate(MigrateCommand::Switch { only, force }) => migrate_switch(cli, only, *force, host, out),
        Command::Migrate(MigrateCommand::Rollback { only }) => migrate_rollback(cli, only, host, out),
        Command::Migrate(MigrateCommand::Finish { now }) => migrate_finish(cli, *now, host, out),
    }
}

impl Cli {
    /// The config and secrets files this invocation uses.
    pub fn paths(&self) -> Paths {
        Paths::new(self.config.clone(), self.secrets.clone())
    }

    /// The logger for this invocation: journal under systemd, stderr otherwise; `--quiet` keeps only
    /// warnings and errors.
    fn logger(&self, host: Arc<dyn Host>) -> Logger {
        let min = if self.quiet { Level::Warning } else { Level::Info };
        Logger::new(host, "moi").with_min_level(min)
    }
}

/// Print a value for `--json`: one pretty-printed document on stdout.
fn print_json(out: &mut dyn Write, value: &serde_json::Value) {
    // A closed pipe (`moi … --json | head`) is not an error worth reporting.
    let _ = writeln!(out, "{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

/// Errors go to stderr for people and, with `--json`, also to stdout as `{"ok": false, "error": …}` so a
/// consumer always gets a document.
fn report_error(cli: &Cli, e: &Error, out: &mut dyn Write) {
    // The command worked and already printed what it found; only the exit code says "problems".
    if matches!(e, Error::ProblemsFound) {
        return;
    }
    let message = match e {
        Error::ConfigInvalid(diagnostics) => {
            for d in diagnostics {
                eprintln!("{d}");
            }
            let n = diagnostics.len();
            format!("configuration is invalid ({n} error{})", if n == 1 { "" } else { "s" })
        }
        other => other.to_string(),
    };
    eprintln!("moi: {message}");
    if cli.json {
        let mut error = json!({ "kind": e.kind(), "message": message, "exit_code": e.exit_code() });
        if let Error::ConfigInvalid(diagnostics) = e {
            error["diagnostics"] = json!(diagnostics);
        }
        print_json(out, &json!({ "ok": false, "error": error }));
    }
}

fn config(cli: &Cli, cmd: &ConfigCommand, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    let paths = cli.paths();
    match cmd {
        ConfigCommand::Path => {
            if cli.json {
                print_json(out, &json!({ "ok": true, "config": paths.config, "secrets": paths.secrets }));
            } else {
                let _ = writeln!(out, "config   {}\nsecrets  {}", paths.config.display(), paths.secrets.display());
            }
        }
        ConfigCommand::Check => {
            let loaded = crate::config::load(host.as_ref(), &paths)?;
            let log = cli.logger(host).with_masker(loaded.masker.clone());
            if cli.json {
                print_json(
                    out,
                    &json!({
                        "ok": true,
                        "files": { "config": paths.config, "secrets": paths.secrets },
                        "secrets_resolved": loaded.secrets_resolved,
                        "warnings": loaded.warnings,
                        "config": loaded.effective_json(),
                    }),
                );
            } else {
                let _ = write!(out, "{}", loaded.effective_toml());
            }
            for w in &loaded.warnings {
                log.warn(&w.to_string(), &[]);
            }
            let secrets = match loaded.secrets_resolved {
                0 => "no secrets used".to_string(),
                1 => "1 secret resolved, masked above".to_string(),
                n => format!("{n} secrets resolved, masked above"),
            };
            log.info(&format!("{}: ok ({secrets})", paths.config.display()), &[]);
        }
    }
    Ok(())
}

fn snapshot(cli: &Cli, args: &SnapshotArgs, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let log = cli.logger(Arc::clone(&host)).with_masker(loaded.masker.clone());
    let state_root = args.state.clone().unwrap_or_else(|| PathBuf::from(&loaded.config.state.dir));
    let state = StateDir::new(state_root);
    state.ensure(host.as_ref())?;
    let db = state.open_db(host.as_ref())?;
    let config = Arc::new(loaded.config);
    let (report, commit) = crate::snapshot::capture(&host, &config, &state, &db, &loaded.masker)?;

    for failed in report.failed() {
        let error = failed.error.as_deref().unwrap_or_default();
        log.warn(&format!("{}: {error} (previous capture kept)", failed.name), &[("collector", &failed.name)]);
    }
    // the bundle goes off-site only if what it holds changed
    let pushed = if args.push { Some(crate::bundle::push(&host, &config, &state, &db, false)?) } else { None };
    let saved = pushed.as_ref().map(|p| *p == crate::bundle::Pushed::Saved);
    if cli.json {
        let mut answer = json!({ "ok": true, "state": state.state(), "snapshot": report, "commit": commit });
        if let Some(saved) = saved {
            answer["pushed"] = json!(saved);
        }
        print_json(out, &answer);
    } else {
        let _ = write!(out, "snapshot saved to {}: {} files, ", state.state().display(), report.stable_files);
        let _ = match &commit {
            Some(c) => writeln!(out, "recorded {}\n  {}", &c.id[..10.min(c.id.len())], c.subject),
            None => writeln!(out, "no configuration changes"),
        };
        match saved {
            Some(true) => {
                let _ = writeln!(out, "state bundle saved off-site");
            }
            Some(false) => {
                let _ = writeln!(out, "state bundle: nothing changed since the last push");
            }
            None => {}
        }
    }
    Ok(())
}

/// The history of this server, or `None` (after saying so) when no snapshot was taken yet.
fn open_history(cli: &Cli, host: &Arc<dyn Host>, out: &mut dyn Write) -> Result<Option<(History, Masker)>, Error> {
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    if !host.exists(&state.state().join(".git")) {
        if cli.json {
            print_json(out, &json!({ "ok": true, "commits": [] }));
        } else {
            let _ = writeln!(out, "no history yet: run `moi snapshot`");
        }
        return Ok(None);
    }
    Ok(Some((History::open(host.as_ref(), &state)?, loaded.masker)))
}

fn log(cli: &Cli, count: usize, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    let Some((history, _)) = open_history(cli, &host, out)? else { return Ok(()) };
    let commits = history.log(count)?;
    if cli.json {
        print_json(out, &json!({ "ok": true, "commits": commits }));
    } else {
        for c in &commits {
            let _ = writeln!(out, "{}  {}", history::display_time(&c.time), c.subject);
        }
    }
    Ok(())
}

fn diff(cli: &Cli, rev: &str, show_secrets: bool, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    if show_secrets && !history::is_root(host.as_ref()) {
        return Err(Error::Usage("--show-secrets needs root (the state dir holds the server's secrets)".into()));
    }
    let Some((history, masker)) = open_history(cli, &host, out)? else { return Ok(()) };
    let (commit, changes) = history.show(rev)?;
    let shown = |text: &str| if show_secrets { text.to_string() } else { masker.mask(&crate::redact::text(text)) };
    let as_text = |bytes: &Option<Vec<u8>>| match bytes {
        None => Some(String::new()),
        Some(b) if !b.iter().take(8192).any(|x| *x == 0) => Some(String::from_utf8_lossy(b).into_owned()),
        Some(_) => None,
    };
    let mut files = Vec::new();
    // The manifest only repeats, as hashes, what the other files show.
    for c in changes.iter().filter(|c| c.path != "snapshot/files.manifest.tsv") {
        let path = history::host_path(&c.path).unwrap_or(&c.path).to_string();
        let body = match (as_text(&c.old), as_text(&c.new)) {
            (Some(old), Some(new)) => shown(&crate::state::diff::unified(&old, &new, 3)),
            _ => "Binary file changed\n".to_string(),
        };
        files.push((c.status, path, body));
    }
    if cli.json {
        let files: Vec<_> =
            files.iter().map(|(status, path, diff)| json!({ "status": status, "path": path, "diff": diff })).collect();
        print_json(out, &json!({ "ok": true, "commit": commit, "files": files, "secrets_shown": show_secrets }));
        return Ok(());
    }
    let when = history::display_time(&commit.time);
    let _ = writeln!(out, "commit {}\nDate:   {when} UTC\n\n    {}\n", commit.id, commit.subject);
    for (status, path, _) in &files {
        let _ = writeln!(out, "{}\t{path}", status.letter());
    }
    for (status, path, body) in &files {
        let (old, new) = match status {
            history::Status::Added => ("/dev/null", path.as_str()),
            history::Status::Deleted => (path.as_str(), "/dev/null"),
            history::Status::Modified => (path.as_str(), path.as_str()),
        };
        let _ = write!(out, "\n--- {old}\n+++ {new}\n{body}");
    }
    if !show_secrets {
        let _ = writeln!(out, "\n(secrets are masked; `sudo moi diff {rev} --show-secrets` shows them)");
    }
    Ok(())
}

fn notes(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    if !host.exists(&state.snapshot()) {
        return Err(Error::Usage(
            "no snapshot yet: run `moi snapshot` first (notes are attached to what it finds)".into(),
        ));
    }
    crate::state::notes::update(host.as_ref(), &state)?;
    let path = state.notes();
    let real = host.real_path(&path).map_err(|e| Error::host(format!("cannot open {}", path.display()), e))?;

    // $VISUAL, then $EDITOR (either may carry arguments), then whatever is installed
    let configured: Vec<String> =
        ["VISUAL", "EDITOR"].iter().filter_map(|v| host.env(v)).filter(|v| !v.trim().is_empty()).collect();
    let candidates: Vec<String> = if configured.is_empty() { vec!["nano".into(), "vi".into()] } else { configured };
    let mut edited = false;
    for candidate in &candidates {
        let mut words = candidate.split_whitespace();
        let Some(program) = words.next() else { continue };
        let cmd = Cmd::new(program).args(words).arg(real.display().to_string());
        match host.run_interactive(&cmd) {
            Ok(0) => {
                edited = true;
                break;
            }
            Ok(code) => return Err(Error::Usage(format!("{program} exited with status {code}: notes not recorded"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::host(format!("cannot run {program}"), e)),
        }
    }
    if !edited {
        return Err(Error::Usage(format!(
            "no editor found ({}): set EDITOR, or edit {} yourself and run `moi snapshot`",
            candidates.join(", "),
            path.display()
        )));
    }
    let commit = history::record(host.as_ref(), &state)?;
    if cli.json {
        print_json(out, &json!({ "ok": true, "notes": path, "commit": commit }));
    } else {
        let _ = match &commit {
            Some(c) => writeln!(out, "notes saved: recorded {}", &c.id[..10.min(c.id.len())]),
            None => writeln!(out, "notes unchanged"),
        };
    }
    Ok(())
}

fn audit(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    let config = Arc::new(loaded.config);
    if !host.exists(&state.snapshot()) {
        state.ensure(host.as_ref())?;
        let db = state.open_db(host.as_ref())?;
        crate::snapshot::capture(&host, &config, &state, &db, &loaded.masker)?;
    }
    let audit = crate::audit::run(&host, &config, &state)?;
    // the context file embeds the audit and lists its gaps under "Attention"
    crate::context::write(host.as_ref(), &config, &state, &loaded.masker)?;
    let gaps = audit.gaps();
    if cli.json {
        print_json(out, &json!({ "ok": true, "gaps": gaps, "file": state.audit(), "audit": audit }));
    } else {
        for section in &audit.sections {
            let lines: Vec<String> = section
                .blocks
                .iter()
                .filter_map(|b| match b {
                    crate::audit::Block::Finding { level: crate::audit::Level::Gap, text } => {
                        Some(format!("  ✗ {text}"))
                    }
                    crate::audit::Block::Finding { level: crate::audit::Level::Review, text } if !cli.quiet => {
                        Some(format!("  review {text}"))
                    }
                    _ => None,
                })
                .collect();
            if !lines.is_empty() {
                let _ = writeln!(out, "{}\n{}", section.title, lines.join("\n"));
            }
        }
        let reviews = audit.findings(crate::audit::Level::Review).len();
        let _ = writeln!(out, "{gaps} gap(s), {reviews} to review — full report: {}", state.audit().display());
    }
    if gaps > 0 { Err(Error::ProblemsFound) } else { Ok(()) }
}

fn context(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let log = cli.logger(Arc::clone(&host)).with_masker(loaded.masker.clone());
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    let config = Arc::new(loaded.config);
    if !host.exists(&state.snapshot()) {
        state.ensure(host.as_ref())?;
        let db = state.open_db(host.as_ref())?;
        crate::snapshot::capture(&host, &config, &state, &db, &loaded.masker)?;
    }
    let written = crate::context::write(host.as_ref(), &config, &state, &loaded.masker)?;
    for failed in &written.failed_copies {
        log.warn(&format!("copy not written: {failed}"), &[]);
    }
    if cli.json {
        print_json(out, &json!({ "ok": true, "context": written }));
    } else {
        let attention = match written.warnings {
            0 => "nothing needs attention".to_string(),
            1 => "1 item needs attention".to_string(),
            n => format!("{n} items need attention"),
        };
        let _ = writeln!(out, "{} written ({} KB, {attention})", written.path.display(), written.bytes.div_ceil(1024));
        for copy in &written.copies {
            let _ = writeln!(out, "copy: {copy}");
        }
    }
    Ok(())
}

fn context_share(
    cli: &Cli,
    ttl: Option<Dur>,
    revoke: bool,
    list: bool,
    host: Arc<dyn Host>,
    out: &mut dyn Write,
) -> Result<(), Error> {
    use crate::context::share;
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let log = cli.logger(Arc::clone(&host)).with_masker(loaded.masker.clone());
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    let config = Arc::new(loaded.config);
    // say "not configured" before touching anything
    share::Bucket::new(&host, &config.context.share)?;
    state.ensure(host.as_ref())?;
    let db = state.open_db(host.as_ref())?;
    if revoke {
        let deleted = share::revoke(&host, &config, &db)?;
        log.log(Level::Notice, "context shares revoked", &[("deleted", &deleted.to_string())]);
        if cli.json {
            print_json(out, &json!({ "ok": true, "revoked": deleted }));
        } else {
            let _ = match deleted {
                0 => writeln!(out, "nothing was shared"),
                1 => writeln!(out, "1 shared copy deleted: its link no longer works"),
                n => writeln!(out, "{n} shared copies deleted: their links no longer work"),
            };
        }
        return Ok(());
    }
    if list {
        let active = share::list(&host, &config, &db)?;
        if cli.json {
            print_json(out, &json!({ "ok": true, "shares": active }));
        } else if active.is_empty() {
            let _ = writeln!(out, "no active shares");
        } else {
            for a in &active {
                let _ = writeln!(out, "{}  expires {} UTC", a.key, history::display_time(&a.expires));
            }
        }
        return Ok(());
    }
    if !host.exists(&state.snapshot()) {
        crate::snapshot::capture(&host, &config, &state, &db, &loaded.masker)?;
    }
    let shared = share::share(&host, &config, &state, &db, &loaded.masker, ttl)?;
    // an unexpected share should be noticed: it is always in the journal
    log.log(
        Level::Notice,
        &format!("context shared, expires {} UTC", history::display_time(&shared.expires)),
        &[("key", &shared.key)],
    );
    // and, with `notify`, on the server's freshness monitor (when there is one)
    if config.context.share.notify {
        let message = format!("context shared, expires {} UTC", history::display_time(&shared.expires));
        let _ = crate::monitors::push(&host, &config, &db, "freshness:snapshot", "up", &message);
    }
    if let Some(lifecycle) =
        shared.lifecycle.as_deref().filter(|l| l.starts_with("not set") || l.starts_with("skipped"))
    {
        log.warn(
            &format!("bucket lifecycle rule {lifecycle}; moi still deletes shared copies itself when they expire"),
            &[],
        );
    }
    if cli.json {
        print_json(out, &json!({ "ok": true, "share": shared }));
    } else {
        let _ = writeln!(
            out,
            "{}\nexpires {} UTC (in {}); `moi context share --revoke` ends it now",
            shared.url,
            history::display_time(&shared.expires),
            shared.ttl
        );
    }
    Ok(())
}

fn check(cli: &Cli, args: &CheckArgs, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::checks;
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    let config = Arc::new(loaded.config);
    let all = checks::enabled(&config);
    let selected: Vec<_> = match &args.name {
        None => all,
        Some(name) => {
            let names: Vec<String> = all.iter().map(|c| c.name()).collect();
            let found: Vec<_> = all.into_iter().filter(|c| c.name() == *name).collect();
            if found.is_empty() {
                let known = if names.is_empty() {
                    "none are enabled in moi.toml".to_string()
                } else {
                    format!("enabled: {}", names.join(", "))
                };
                return Err(Error::Usage(format!("no such check: {name} ({known})")));
            }
            found
        }
    };
    state.ensure(host.as_ref())?;
    let db = state.open_db(host.as_ref())?;
    let mut results = Vec::new();
    for c in &selected {
        // a manual run looks without recording, so the daemon's deltas and counters stay as they are
        let mut result = checks::run(&host, &config, &db, c, checks::Mode::Dry)?;
        if args.push {
            result.pushed = Some(checks::push(&host, &config, &db, &result));
            let line = format!("{}: {}", result.status.as_str(), result.message);
            Logger::new(Arc::clone(&host), &c.journal_tag(&config))
                .with_masker(loaded.masker.clone())
                .info(&line, &[("check", &result.name)]);
        }
        results.push(result);
    }
    let down = results.iter().filter(|r| r.status == checks::Status::Down).count();
    if cli.json {
        print_json(out, &json!({ "ok": true, "down": down, "checks": results }));
    } else {
        if results.is_empty() {
            let _ = writeln!(
                out,
                "no checks are enabled: add [checks.resources], [checks.storage], [checks.services] or [[checks.custom]] to moi.toml"
            );
        }
        for r in &results {
            // one check prints exactly what the legacy script printed; several are told apart by name
            let prefix = if results.len() > 1 { format!("[{}] ", r.name) } else { String::new() };
            let _ = writeln!(out, "{prefix}{}: {}", r.status.as_str(), r.message);
            if let Some(pushed) = r.pushed.as_deref().filter(|p| *p != "ok") {
                let _ = writeln!(out, "{prefix}{pushed}");
            }
        }
    }
    if down > 0 { Err(Error::ProblemsFound) } else { Ok(()) }
}

fn monitors(cli: &Cli, command: &MonitorsCommand, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::monitors::{self, Options};
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    state.ensure(host.as_ref())?;
    let db = state.open_db(host.as_ref())?;
    let docker = crate::docker::Docker::new(Arc::clone(&host));
    match command {
        MonitorsCommand::Sync { dry_run, update, prune } => {
            let options = Options { update: *update, prune: *prune };
            let config = &loaded.config;
            if *dry_run {
                // a dry run needs no login: Kuma's own database says what it has
                let inventory = monitors::local_inventory(host.as_ref(), config, &docker)?;
                let plan = monitors::plan_for(host.as_ref(), config, &docker, &state, &db, &inventory, options)?;
                if cli.json {
                    print_json(
                        out,
                        &json!({ "ok": true, "dry_run": true, "changes": plan.changes(), "plan": plan.steps, "warnings": plan.warnings }),
                    );
                } else {
                    let _ = write!(out, "{}", plan.render());
                    for warning in &plan.warnings {
                        let _ = writeln!(out, "warning: {warning}");
                    }
                }
                return Ok(());
            }
            let mut kuma = monitors::connect(&host, config)?;
            let inventory = kuma.inventory().map_err(Error::Monitors)?;
            let plan = monitors::plan_for(host.as_ref(), config, &docker, &state, &db, &inventory, options)?;
            let report = monitors::apply::apply(&host, config, &docker, &db, &mut kuma, &plan, &inventory)?;
            let log = Logger::new(Arc::clone(&host), "moi").with_masker(loaded.masker.clone());
            if plan.changes() > 0 {
                log.info(&format!("monitors sync: {}", report.summary()), &[("job", "monitors-sync")]);
            }
            if cli.json {
                print_json(
                    out,
                    &json!({ "ok": true, "dry_run": false, "changes": plan.changes(), "plan": plan.steps, "result": report, "warnings": plan.warnings, "kuma": kuma.version }),
                );
            } else {
                // what was done, without the monitors that are none of moi's business
                let rows: Vec<[String; 4]> = plan
                    .steps
                    .iter()
                    .filter(|s| s.owner.is_some())
                    .map(|s| [s.action.as_str().to_string(), s.monitor.clone(), s.kind.clone(), s.detail.clone()])
                    .collect();
                if !rows.is_empty() {
                    let _ = write!(out, "{}", monitors::table(["ACTION", "MONITOR", "TYPE", "DETAIL"], &rows));
                }
                for failure in &report.failed {
                    let _ = writeln!(out, "failed: {}: {}", failure.monitor, failure.error);
                }
                for warning in &plan.warnings {
                    let _ = writeln!(out, "warning: {warning}");
                }
                let _ = writeln!(out, "{}", report.summary());
            }
            if report.failed.is_empty() { Ok(()) } else { Err(Error::ProblemsFound) }
        }
        MonitorsCommand::List => {
            let inventory = monitors::local_inventory(host.as_ref(), &loaded.config, &docker)?;
            let plan = monitors::plan_for(
                host.as_ref(),
                &loaded.config,
                &docker,
                &state,
                &db,
                &inventory,
                Options::default(),
            )?;
            // who a monitor belongs to: its binding, or what a sync would bind it to
            let owner = |id: i64| {
                plan.steps
                    .iter()
                    .find(|s| s.kuma_id == Some(id) && s.owner.is_some())
                    .map(|s| (s.action, s.owner.clone().unwrap_or_default()))
            };
            let rows: Vec<serde_json::Value> = inventory
                .monitors
                .iter()
                .map(|m| {
                    let status = if m.active { m.last_status.clone().unwrap_or_else(|| "no data".to_string()) } else { "paused".to_string() };
                    let (bound, owner) = match owner(m.id) {
                        Some((monitors::Action::Keep, owner)) => (true, Some(owner)),
                        Some((_, owner)) => (false, Some(owner)),
                        None => (false, None),
                    };
                    json!({ "id": m.id, "name": m.name, "type": m.kind, "status": status, "owner": owner, "bound": bound, "watchdog": m.is_watchdog() })
                })
                .collect();
            if cli.json {
                print_json(out, &json!({ "ok": true, "db": inventory.db, "monitors": rows }));
            } else {
                let cells: Vec<[String; 4]> = rows
                    .iter()
                    .map(|r| {
                        let owner = match (r["owner"].as_str(), r["bound"].as_bool()) {
                            (Some(owner), Some(true)) => owner.to_string(),
                            (Some(owner), _) => format!("{owner} (not bound yet: moi monitors sync)"),
                            (None, _) if r["watchdog"] == true => "- (watchdog)".to_string(),
                            (None, _) => "-".to_string(),
                        };
                        [
                            r["name"].as_str().unwrap_or_default().to_string(),
                            r["type"].as_str().unwrap_or_default().to_string(),
                            r["status"].as_str().unwrap_or_default().to_string(),
                            owner,
                        ]
                    })
                    .collect();
                let _ = write!(out, "{}", monitors::table(["MONITOR", "TYPE", "STATUS", "OWNER"], &cells));
            }
            Ok(())
        }
    }
}

fn status(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    let asked = crate::daemon::ask(host.as_ref(), &json!({ "cmd": "status" }));
    let Some(status) = asked else {
        // no daemon: say so, and show what the last one left in moi.db
        let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
        let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
        let mut last: Vec<serde_json::Value> = Vec::new();
        if host.exists(&state.db()) {
            let db = state.open_db(host.as_ref())?;
            let mut stmt = db
                .conn()
                .prepare("SELECT job, last_run, last_status FROM schedule_runs ORDER BY job")
                .map_err(|e| Error::Db(e.to_string()))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?)))
                .map_err(|e| Error::Db(e.to_string()))?;
            for (job, at, status) in rows.flatten() {
                let at = crate::host::format_rfc3339(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(at).unwrap_or(0)),
                );
                last.push(json!({ "name": job, "last": { "at": at, "status": status } }));
            }
        }
        if cli.json {
            print_json(out, &json!({ "ok": true, "running": false, "jobs": last }));
        } else {
            let _ = writeln!(out, "daemon: not running (systemctl status moi)");
            for job in &last {
                let _ = writeln!(
                    out,
                    "  {:<18} last {} at {} UTC",
                    job["name"].as_str().unwrap_or_default(),
                    job["last"]["status"].as_str().unwrap_or("?"),
                    history::display_time(job["last"]["at"].as_str().unwrap_or_default())
                );
            }
        }
        return Err(Error::ProblemsFound);
    };
    if cli.json {
        let mut status = status;
        status["ok"] = json!(true);
        print_json(out, &status);
        return Ok(());
    }
    let when = |v: &serde_json::Value| v.as_str().map(history::display_time).unwrap_or_default();
    let _ = writeln!(
        out,
        "daemon: running since {} UTC (moi {})",
        when(&status["started"]),
        status["version"].as_str().unwrap_or_default()
    );
    let mut down = 0;
    for job in status["jobs"].as_array().into_iter().flatten() {
        let name = job["name"].as_str().unwrap_or_default();
        let last = &job["last"];
        let state = match (job["running"].as_bool(), last["status"].as_str()) {
            (Some(true), _) => "running".to_string(),
            (_, Some(status)) => format!("{status}: {}", last["message"].as_str().unwrap_or_default()),
            _ => "not run yet".to_string(),
        };
        if matches!(last["status"].as_str(), Some("down" | "failed")) {
            down += 1;
        }
        let _ = writeln!(out, "  {name:<18} {state}");
        if job["next"].is_string() {
            let _ = writeln!(out, "  {:<18} next {} UTC", "", when(&job["next"]));
        }
    }
    if down > 0 { Err(Error::ProblemsFound) } else { Ok(()) }
}

/// How long someone applying rules over SSH has to say they can still get in.
fn firewall(cli: &Cli, command: &FirewallCommand, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::firewall::{self, Family, State};
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let config = &loaded.config;
    let state = StateDir::new(PathBuf::from(&config.state.dir));
    state.ensure(host.as_ref())?;
    let db = state.open_db(host.as_ref())?;
    if !config.firewall.manage {
        if cli.json {
            print_json(out, &json!({ "ok": true, "managed": false }));
        } else {
            let _ = writeln!(out, "the firewall is not managed by moi (firewall.manage = false in moi.toml)");
        }
        return if matches!(command, FirewallCommand::Status) {
            Ok(())
        } else {
            Err(Error::Usage("set firewall.manage = true and the rules files in moi.toml first".to_string()))
        };
    }
    match command {
        FirewallCommand::Status => {
            let mut states = Vec::new();
            for family in Family::ALL {
                states.push((family, firewall::state(host.as_ref(), config, &db, family)?));
            }
            if cli.json {
                let families: Vec<_> = states.iter().map(|(f, s)| json!({ "family": f, "state": s })).collect();
                print_json(
                    out,
                    &json!({ "ok": true, "managed": true, "chains": config.firewall.chains, "families": families }),
                );
            } else {
                let _ = writeln!(out, "managed chains: {}", config.firewall.chains.join(", "));
                for (family, state) in &states {
                    let _ = writeln!(out, "{}: {}", family.name(), state.describe());
                }
            }
            if states.iter().all(|(_, s)| matches!(s, State::InSync | State::Unavailable)) {
                Ok(())
            } else {
                Err(Error::ProblemsFound)
            }
        }
        FirewallCommand::Diff => {
            let mut all = String::new();
            for family in Family::ALL {
                all.push_str(&firewall::diff(host.as_ref(), config, &db, family)?);
            }
            if cli.json {
                print_json(out, &json!({ "ok": true, "differs": !all.is_empty(), "diff": all }));
            } else if all.is_empty() {
                let _ = writeln!(
                    out,
                    "no difference: the managed chains are what moi applied, and the rules files have not changed"
                );
            } else {
                let _ = write!(out, "{all}");
            }
            if all.is_empty() { Ok(()) } else { Err(Error::ProblemsFound) }
        }
        FirewallCommand::Apply => {
            let log = Logger::new(Arc::clone(&host), "moi").with_masker(loaded.masker.clone());
            let firewall::Guarded { applied, saved } = match firewall::apply_guarded(&host, config, &state, &db) {
                Ok(done) => done,
                Err(e) => {
                    if matches!(&e, Error::Usage(why) if why.starts_with("not confirmed")) {
                        log.warn("firewall: rules applied over SSH were not confirmed and have been rolled back", &[]);
                    }
                    return Err(e);
                }
            };
            let summary: Vec<String> =
                applied.iter().map(|a| format!("{} rules ({})", a.rules, a.family.name())).collect();
            log.info(
                &format!("firewall: applied {} to {}", summary.join(", "), config.firewall.chains.join(", ")),
                &[],
            );
            if cli.json {
                print_json(
                    out,
                    &json!({ "ok": true, "applied": applied, "saved": saved.iter().map(|(_, f)| f).collect::<Vec<_>>() }),
                );
            } else {
                let _ = writeln!(out, "applied {} to {}", summary.join(", "), config.firewall.chains.join(", "));
                let _ = writeln!(out, "previous rules saved in {}", state.archive().join("firewall").display());
            }
            Ok(())
        }
    }
}

fn init(cli: &Cli, args: &InitArgs, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::init;
    let paths = cli.paths();
    let found = init::discover(&host);
    // an existing configuration is the owner's: never overwritten, only compared
    if host.exists(&paths.config) {
        let loaded = crate::config::load(host.as_ref(), &paths)?;
        let suggestions = init::suggestions(&found, &loaded.config);
        if cli.json {
            print_json(
                out,
                &json!({ "ok": true, "written": false, "config": paths.config, "suggestions": suggestions }),
            );
        } else {
            let _ = writeln!(out, "{} exists and was not changed.", paths.config.display());
            if suggestions.is_empty() {
                let _ = writeln!(out, "It covers everything `moi init` would propose for this server.");
            } else {
                let _ = writeln!(out, "What `moi init` would propose that it does not have:");
                for suggestion in &suggestions {
                    let _ = writeln!(out, "  {suggestion}");
                }
            }
        }
        // a configuration that was written by hand, or came back with a restore, still wants its
        // service: installed here when there is none yet
        // (only where the binary is installed where a service can start it)
        if !args.observe && !host.exists(Path::new(init::UNIT_PATH)) && init::binary(host.as_ref()).is_ok() {
            let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
            state.ensure(host.as_ref())?;
            if !host.exists(&state.snapshot()) {
                let db = state.open_db(host.as_ref())?;
                crate::snapshot::capture(&host, &Arc::new(loaded.config.clone()), &state, &db, &loaded.masker)?;
            }
            let binary = init::install_service(host.as_ref())?;
            if !cli.json {
                let _ = writeln!(out, "moi.service installed and started ({binary} daemon)");
            }
        }
        return Ok(());
    }
    // nothing is written if the service could not be installed afterwards
    if !args.observe {
        init::binary(host.as_ref())?;
    }
    let text = init::config_text(&found, args.observe);
    let dir = paths.config.parent().unwrap_or(Path::new("/etc/moi"));
    host.create_dir_all(dir, 0o755).map_err(|e| Error::host(format!("cannot create {}", dir.display()), e))?;
    host.write(&paths.config, text.as_bytes(), 0o644)
        .map_err(|e| Error::host(format!("cannot write {}", paths.config.display()), e))?;
    if !host.exists(&paths.secrets) {
        let note = "# secrets.toml — the values behind the `secret:NAME` references in moi.toml, as NAME = \"value\".\n# Only root can read this file; moi never prints what is in it.\n";
        host.write(&paths.secrets, note.as_bytes(), 0o600)
            .map_err(|e| Error::host(format!("cannot write {}", paths.secrets.display()), e))?;
    }
    // what was written must be a configuration moi accepts
    let loaded = crate::config::load(host.as_ref(), &paths)?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    state.ensure(host.as_ref())?;
    let db = state.open_db(host.as_ref())?;
    let config = Arc::new(loaded.config);
    let (report, _) = crate::snapshot::capture(&host, &config, &state, &db, &loaded.masker)?;
    // the service last: its first act is a check cycle, and it should find the snapshot there
    let service = if args.observe { None } else { Some(init::install_service(host.as_ref())?) };
    if cli.json {
        print_json(
            out,
            &json!({ "ok": true, "written": true, "config": paths.config, "secrets": paths.secrets, "found": found, "snapshot_files": report.stable_files, "service": service }),
        );
        return Ok(());
    }
    let _ = writeln!(out, "wrote {} and {}", paths.config.display(), paths.secrets.display());
    let mut seen = vec![format!("{} disks", found.disks.len()), format!("{} data roots", found.data_roots.len())];
    if found.docker {
        seen.push("Docker".to_string());
    }
    if let Some(url) = &found.kuma_url {
        seen.push(format!("Uptime Kuma at {url}"));
    }
    let _ = writeln!(out, "found: {}", seen.join(", "));
    let _ = writeln!(out, "first snapshot: {} files in {}", report.stable_files, state.state().display());
    match &service {
        Some(binary) => {
            let _ = writeln!(out, "moi.service installed and started ({binary} daemon)");
        }
        None => {
            let _ = writeln!(out, "no service installed (--observe): `moi snapshot` updates the state when you run it");
        }
    }
    let _ = writeln!(out, "next:");
    let _ = writeln!(out, "  moi audit                      what is not backed up, or would not survive a reboot");
    let _ = writeln!(out, "  less {}", state.context().display());
    if found.kuma_url.is_some() && !args.observe {
        let _ = writeln!(out, "  moi monitors sync --dry-run    what Uptime Kuma would be given");
    }
    if !found.legacy.is_empty() {
        let _ = writeln!(out, "this server already looks after itself:");
        for sign in &found.legacy {
            let _ = writeln!(out, "  {sign}");
        }
        let _ = writeln!(out, "`moi migrate plan` shows how moi can take that over without breaking a monitor.");
    }
    Ok(())
}

fn uninstall(cli: &Cli, purge: bool, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    // a configuration that no longer loads must not keep moi from being removed
    let loaded = crate::config::load(host.as_ref(), &cli.paths()).ok();
    let config = loaded.as_ref().map(|l| &l.config);
    let state = StateDir::new(config.map_or_else(|| PathBuf::from("/var/lib/moi"), |c| PathBuf::from(&c.state.dir)));
    if purge {
        // history, archives and bindings cannot be brought back: the one who asks must mean this server
        let name = config.map_or_else(|| crate::host::short_hostname(host.as_ref()), |c| c.server.name.clone());
        let prompt = format!(
            "This deletes {} for good: the server's recorded history, the archive of everything moi replaced, and moi.db.\nType the server's name ({name}) to go on: ",
            state.root().display()
        );
        let answer = host.ask(&prompt, std::time::Duration::from_secs(120)).ok().flatten();
        if answer.as_deref() != Some(name.as_str()) {
            return Err(Error::Usage("not confirmed: nothing was removed".to_string()));
        }
    }
    let mut done = crate::init::uninstall(host.as_ref(), config);
    if purge && host.exists(state.root()) {
        crate::init::remove_tree(host.as_ref(), state.root())
            .map_err(|e| Error::host(format!("cannot delete {}", state.root().display()), e))?;
        done.push(format!("deleted {}", state.root().display()));
    }
    let config_dir = cli.paths().config.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let mut kept = vec![format!("{config_dir} (your configuration and secrets)")];
    if !purge && host.exists(state.root()) {
        kept.push(format!("{} (history and archive; `moi uninstall --purge` deletes them)", state.root().display()));
    }
    if cli.json {
        print_json(out, &json!({ "ok": true, "removed": done, "kept": kept }));
    } else {
        if done.is_empty() {
            let _ = writeln!(out, "nothing of moi was installed");
        }
        for line in &done {
            let _ = writeln!(out, "{line}");
        }
        for line in &kept {
            let _ = writeln!(out, "kept: {line}");
        }
        let _ = writeln!(
            out,
            "Monitors in Uptime Kuma, firewall rules in the kernel and backups in their repository are untouched."
        );
    }
    Ok(())
}

fn backup(cli: &Cli, args: &BackupArgs, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::backup::{plan::Plan, run};
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    state.ensure(host.as_ref())?;
    let db = state.open_db(host.as_ref())?;
    let config = Arc::new(loaded.config);
    if args.drill {
        let report = crate::backup::drill::run(&host, &config, &state, &db)?;
        if cli.json {
            print_json(out, &json!({ "ok": true, "drill": report }));
        } else {
            let _ = writeln!(out, "drill of {}", report.dir);
            for line in &report.lines {
                let _ = writeln!(out, "  {line}");
            }
            let _ = writeln!(
                out,
                "{}",
                if report.ok { "the backup can be restored" } else { "THE BACKUP COULD NOT BE RESTORED" }
            );
        }
        return if report.ok { Ok(()) } else { Err(Error::ProblemsFound) };
    }
    if !args.dry_run {
        let json = cli.json;
        let report = run::run(&host, &config, &state, &db, &loaded.masker, &mut |line| {
            if !json {
                let _ = writeln!(out, "{line}");
            }
        })?;
        if cli.json {
            print_json(out, &json!({ "ok": true, "backup": report }));
        }
        return if report.ok { Ok(()) } else { Err(Error::ProblemsFound) };
    }
    // ---- dry run: the plan and the guards, nothing written to the backup directory
    if !host.exists(&state.snapshot()) {
        crate::snapshot::capture(&host, &config, &state, &db, &loaded.masker)?;
    }
    let docker = crate::docker::Docker::new(Arc::clone(&host));
    let view = crate::snapshot::View::new(host.as_ref(), &state);
    let (plan, _) = Plan::make(host.as_ref(), &config, &docker, &view);
    let backup = &config.backup;
    let mut guards: Vec<(bool, String)> = Vec::new();
    if !backup.enabled {
        guards.push((false, "backups are not switched on: there is no [backup] section in moi.toml".to_string()));
    }
    if let Some(mount) = &backup.require_mount {
        let mounted = crate::snapshot::mounts(host.as_ref()).iter().any(|m| m.target == *mount);
        guards.push((mounted, format!("{mount} is {}mounted", if mounted { "" } else { "not " })));
    }
    if let Some(dir) = &backup.local_dir {
        let total = run::tree_size(host.as_ref(), Path::new(dir));
        let limit = backup.max_local.bytes();
        guards.push((
            total <= limit,
            format!("local backups use {} of {} allowed", crate::audit::human(total), crate::audit::human(limit)),
        ));
    }
    for dump in plan.dumps.iter().filter(|d| d.container.is_none()) {
        guards.push((false, format!("{} is not running: it cannot be dumped", dump.name)));
    }
    let failing = guards.iter().filter(|(ok, _)| !ok).count();
    if cli.json {
        let guards: Vec<_> = guards.iter().map(|(ok, text)| json!({ "ok": ok, "guard": text })).collect();
        print_json(out, &json!({ "ok": true, "dry_run": true, "plan": plan, "guards": guards }));
    } else {
        let _ = writeln!(out, "databases (dumped through docker exec, compressed):");
        for dump in &plan.dumps {
            let state = if dump.container.is_some() { String::new() } else { "  NOT RUNNING".to_string() };
            let _ = writeln!(out, "  {:<28} {:<9} user {}{state}", dump.name, dump.engine, dump.user);
        }
        let _ = writeln!(out, "paths:");
        for root in &plan.roots {
            let _ = writeln!(
                out,
                "  {:<52} {:>9}  {:>6} files  {}",
                root.path,
                crate::audit::human(root.bytes),
                root.files,
                root.why
            );
        }
        if !plan.missing.is_empty() {
            let _ = writeln!(out, "not on this machine (skipped):");
            for path in &plan.missing {
                let _ = writeln!(out, "  {path}");
            }
        }
        let _ = writeln!(out, "left out:");
        for (path, why) in &plan.excluded {
            let _ = writeln!(out, "  {path:<52} {why}");
        }
        let _ = writeln!(out, "guards:");
        for (ok, text) in &guards {
            let _ = writeln!(out, "  {} {text}", if *ok { "ok  " } else { "FAIL" });
        }
    }
    if failing > 0 { Err(Error::ProblemsFound) } else { Ok(()) }
}

fn push(cli: &Cli, force: bool, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::bundle::{self, Pushed};
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    state.ensure(host.as_ref())?;
    let db = state.open_db(host.as_ref())?;
    let pushed = bundle::push(&host, &loaded.config, &state, &db, force)?;
    let said = match pushed {
        Pushed::Saved => "state bundle saved off-site",
        Pushed::Unchanged => "nothing changed since the last push (use --force to push anyway)",
    };
    if cli.json {
        print_json(out, &json!({ "ok": true, "pushed": pushed == Pushed::Saved }));
    } else {
        let _ = writeln!(out, "{said}");
    }
    Ok(())
}

fn pull(cli: &Cli, repository: Option<&str>, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::backup::offsite::Restic;
    // a machine being rebuilt has no configuration yet; one that has, has the repository in it
    let loaded = crate::config::load(host.as_ref(), &cli.paths()).ok();
    let offsite = loaded.as_ref().map(|l| &l.config.backup.offsite).filter(|o| o.enabled);
    let restic = match (repository, offsite) {
        (None, Some(offsite)) => Restic::new(&host, offsite)?,
        (Some(url), configured) => {
            // the password and the access keys come from the environment — or are asked for
            let mut env = vec![("RESTIC_REPOSITORY".to_string(), url.to_string())];
            let have_password = host.env("RESTIC_PASSWORD").is_some() || host.env("RESTIC_PASSWORD_FILE").is_some();
            if let Some(offsite) = configured.filter(|o| o.repository.as_ref().is_some_and(|r| r.expose() == url)) {
                return pull_with(cli, Restic::new(&host, offsite)?, &host, loaded.as_ref(), out);
            }
            if !have_password {
                let prompt = "Repository password (it will be visible as you type; set RESTIC_PASSWORD or RESTIC_PASSWORD_FILE to avoid that): ";
                match host.ask(prompt, std::time::Duration::from_secs(300)).ok().flatten().filter(|p| !p.is_empty()) {
                    Some(password) => env.push(("RESTIC_PASSWORD".to_string(), password)),
                    None => {
                        return Err(Error::Usage(
                            "the repository's password is needed: set RESTIC_PASSWORD or RESTIC_PASSWORD_FILE (and the repository's access keys, e.g. AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY) and run again"
                                .to_string(),
                        ));
                    }
                }
            }
            Restic::with_env(&host, env)
        }
        (None, None) => {
            return Err(Error::Usage(
                "which repository? `moi pull --repository <URL>` (the URL is on your recovery card)".to_string(),
            ));
        }
    };
    pull_with(cli, restic, &host, loaded.as_ref(), out)
}

fn pull_with(
    cli: &Cli,
    restic: crate::backup::offsite::Restic,
    host: &Arc<dyn Host>,
    loaded: Option<&crate::config::Loaded>,
    out: &mut dyn Write,
) -> Result<(), Error> {
    let root = loaded.map_or_else(|| PathBuf::from("/var/lib/moi"), |l| PathBuf::from(&l.config.state.dir));
    host.create_dir_all(&root, 0o700).map_err(|e| Error::host(format!("cannot create {}", root.display()), e))?;
    let pulled = crate::bundle::pull(host, &restic, &root)?;
    if cli.json {
        print_json(out, &json!({ "ok": true, "dir": pulled.dir, "host": pulled.host, "time": pulled.time }));
    } else {
        let _ = writeln!(
            out,
            "state bundle of {} from {} restored to {}",
            pulled.host,
            history::display_time(&pulled.time),
            pulled.dir
        );
        let _ = writeln!(out, "next: moi restore --bundle {} --dry-run", pulled.dir);
    }
    Ok(())
}

fn restore(cli: &Cli, args: &RestoreArgs, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::restore::{self, Options, PHASES};
    if args.list {
        if cli.json {
            let phases: Vec<_> = PHASES.iter().map(|(name, what)| json!({ "name": name, "what": what })).collect();
            print_json(out, &json!({ "ok": true, "phases": phases }));
        } else {
            for (name, what) in PHASES {
                let _ = writeln!(out, "{name:<14} {what}");
            }
        }
        return Ok(());
    }
    let Some(bundle) = &args.bundle else {
        return Err(Error::Usage(
            "which bundle? `moi pull` downloads the latest one and prints its directory; then `moi restore --bundle DIR --dry-run`".to_string(),
        ));
    };
    let options = Options {
        dry_run: args.dry_run,
        only: args.only.clone(),
        from: args.from.clone(),
        skip: args.skip.clone(),
        yes: args.yes,
        snapshot: args.snapshot.clone(),
    };
    if cli.json {
        // the phases talk to a person as they go; a machine gets the transcript and the count
        let mut transcript = Vec::new();
        let warnings = restore::restore(&host, bundle, &options, &mut transcript)?;
        let lines: Vec<String> = String::from_utf8_lossy(&transcript).lines().map(str::to_string).collect();
        print_json(out, &json!({ "ok": true, "dry_run": args.dry_run, "warnings": warnings, "log": lines }));
    } else {
        restore::restore(&host, bundle, &options, out)?;
    }
    Ok(())
}

fn migrate_plan(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    // the plan is read off the last snapshot: what calls what, which script pushes where
    if !host.exists(&state.snapshot()) {
        return Err(Error::Usage(
            "no snapshot yet: run `moi snapshot` first — the plan is made from what it records".to_string(),
        ));
    }
    let plan = crate::migrate::plan(host.as_ref(), &loaded.config, &state);
    if cli.json {
        print_json(out, &json!({ "ok": true, "items": plan.items, "untouched": plan.untouched }));
    } else {
        let _ = write!(out, "{}", loaded.masker.mask(&crate::migrate::render(&plan)));
    }
    Ok(())
}

fn migrate_shadow(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::migrate::{self, shadow};
    // first, because a configuration that refers to a secret nobody has entered yet does not load
    let secrets = shadow::import_secrets(host.as_ref(), &cli.paths())?;
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    if !host.exists(&state.snapshot()) {
        return Err(Error::Usage(
            "no snapshot yet: run `moi snapshot` first — the plan is made from what it records".to_string(),
        ));
    }
    let db = state.open_db(host.as_ref())?;
    let plan = migrate::plan(host.as_ref(), &loaded.config, &state);
    if plan.items.is_empty() {
        let _ =
            writeln!(out, "nothing to migrate: no legacy script, cron job or unit does what moi is configured to do");
        return Ok(());
    }
    shadow::start(&db, &plan.items);
    let imported = shadow::import(host.as_ref(), &loaded.config, &db, &plan.items)?;
    let switched = shadow::switched(&db);
    let held: Vec<&str> =
        plan.items.iter().map(|i| i.name.as_str()).filter(|n| !switched.iter().any(|s| s == n)).collect();
    let daemon = host.exists(Path::new(crate::daemon::SOCKET));
    if cli.json {
        print_json(
            out,
            &json!({ "ok": true, "held": held, "switched": switched, "imported": imported, "secrets": secrets, "daemon": daemon }),
        );
        return Ok(());
    }
    let _ = writeln!(out, "shadow: the legacy setup keeps doing these, moi runs alongside and only watches");
    for name in &held {
        let _ = writeln!(out, "  held      {name}");
    }
    for name in &switched {
        let _ = writeln!(out, "  switched  {name}");
    }
    if !secrets.is_empty() {
        let _ = writeln!(
            out,
            "secrets taken from the legacy scripts into {}: {}",
            cli.paths().secrets.display(),
            secrets.join(", ")
        );
    }
    for line in &imported {
        let _ = writeln!(out, "imported  {line}");
    }
    if !daemon {
        let _ =
            writeln!(out, "the daemon is not running: `systemctl start moi` — without it there is nothing to compare");
    }
    let _ = writeln!(out, "next: let both run for an hour, then `moi migrate compare`");
    Ok(())
}

fn migrate_compare(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::migrate::compare;
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    let db = state.open_db(host.as_ref())?;
    let docker = crate::docker::Docker::new(Arc::clone(&host));
    let all = compare::compare(host.as_ref(), &loaded.config, &state, &db, &docker);
    if cli.json {
        print_json(out, &json!({ "ok": true, "checks": all }));
    } else {
        let _ = write!(out, "{}", loaded.masker.mask(&compare::render(&all)));
    }
    if all.iter().any(|c| c.count(compare::Verdict::Different) > 0) { Err(Error::ProblemsFound) } else { Ok(()) }
}

/// What the three commands that change the legacy setup need: configuration, state, database, plan.
fn migration(
    cli: &Cli,
    host: &Arc<dyn Host>,
) -> Result<(crate::config::Loaded, StateDir, crate::state::db::Db, crate::migrate::Plan), Error> {
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    if !host.exists(&state.snapshot()) {
        return Err(Error::Usage(
            "no snapshot yet: run `moi snapshot` first — the plan is made from what it records".to_string(),
        ));
    }
    let db = state.open_db(host.as_ref())?;
    let plan = crate::migrate::plan(host.as_ref(), &loaded.config, &state);
    Ok((loaded, state, db, plan))
}

fn migrate_switch(
    cli: &Cli,
    only: &[String],
    force: bool,
    host: Arc<dyn Host>,
    out: &mut dyn Write,
) -> Result<(), Error> {
    use crate::migrate::switch;
    let (loaded, state, db, plan) = migration(cli, &host)?;
    let cx = switch::Cx { host: &host, config: &loaded.config, state: &state, db: &db };
    let (lines, refused) = switch::switch(&cx, &plan, only, force)?;
    let held: Vec<String> =
        plan.items.iter().map(|i| i.name.clone()).filter(|n| crate::migrate::shadow::held(&db, n)).collect();
    if cli.json {
        print_json(out, &json!({ "ok": !refused, "items": lines, "held": held }));
    } else {
        for line in &lines {
            let _ = writeln!(out, "{}", loaded.masker.mask(line));
        }
        if held.is_empty() {
            let _ = writeln!(
                out,
                "everything is moi's now. `moi migrate rollback` takes it back; `moi migrate finish` removes the legacy files after a week"
            );
        } else {
            let _ = writeln!(out, "still with the legacy setup: {}", held.join(", "));
        }
    }
    if refused { Err(Error::ProblemsFound) } else { Ok(()) }
}

fn migrate_rollback(cli: &Cli, only: &[String], host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::migrate::switch;
    let (loaded, state, db, plan) = migration(cli, &host)?;
    let cx = switch::Cx { host: &host, config: &loaded.config, state: &state, db: &db };
    let lines = switch::rollback(&cx, &plan, only)?;
    if cli.json {
        print_json(out, &json!({ "ok": true, "items": lines }));
    } else {
        for line in &lines {
            let _ = writeln!(out, "{line}");
        }
    }
    Ok(())
}

fn migrate_finish(cli: &Cli, now: bool, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::migrate::switch;
    let (loaded, state, db, plan) = migration(cli, &host)?;
    let cx = switch::Cx { host: &host, config: &loaded.config, state: &state, db: &db };
    let mut lines = switch::finish(&cx, &plan, now)?;
    // the record of the server follows what was just removed
    let config = Arc::new(loaded.config.clone());
    match crate::snapshot::capture(&host, &config, &state, &db, &loaded.masker) {
        Ok(_) => lines.push("snapshot taken: the legacy setup is out of the server's description".to_string()),
        Err(e) => lines.push(format!("the snapshot afterwards failed ({e}): run `moi snapshot`")),
    }
    // and nothing must be missing in Kuma because of it
    if loaded.config.monitors.kuma.enabled {
        let docker = crate::docker::Docker::new(Arc::clone(&host));
        let plan = crate::monitors::local_inventory(host.as_ref(), &loaded.config, &docker).and_then(|inventory| {
            crate::monitors::plan_for(
                host.as_ref(),
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
                let create = plan.steps.iter().filter(|s| s.action == crate::monitors::Action::Create).count();
                lines.push(if create == 0 {
                    "monitors: every check reports to the monitor it had".to_string()
                } else {
                    format!("monitors: {create} would be created — look at `moi monitors sync --dry-run`")
                });
            }
            Err(e) => lines.push(format!("monitors could not be looked at ({e})")),
        }
    }
    if cli.json {
        print_json(out, &json!({ "ok": true, "done": lines }));
    } else {
        for line in &lines {
            let _ = writeln!(out, "{line}");
        }
    }
    Ok(())
}

fn devbox(cli: &Cli, command: &DevboxCommand, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::devbox;
    use devbox_core::setup::{Routing, UpOptions};
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    if loaded.config.devbox.enabled == Some(false) {
        return Err(Error::Usage(
            "the devbox integration is switched off ([devbox] enabled = false in moi.toml)".to_string(),
        ));
    }
    // nothing set up yet: only `up` makes sense, and says what it would do
    if !matches!(command, DevboxCommand::Up(_)) && !host.exists(Path::new(devbox::CONFIG)) {
        return Err(Error::Usage(format!(
            "devbox is not set up on this server (no {}).\n`moi devbox up --domain <your domain> --dry-run` shows what setting it up would do: an Incus VM that sleeps when idle, VS Code and an agents app in the browser — see https://github.com/prabhask5/devbox",
            devbox::CONFIG
        )));
    }
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    let cx = devbox::Cx { host: &host, config: &loaded.config, state: &state };
    let json = cli.json;
    let done = match command {
        DevboxCommand::Up(args) => {
            state.ensure(host.as_ref())?;
            let options = UpOptions {
                domain: args.domain.clone(),
                code_host: args.code_host.clone(),
                agents_host: args.agents_host.clone(),
                instance: args.instance.clone(),
                adopt: args.adopt,
                cpu: args.cpu,
                memory: args.memory.clone(),
                disk: args.disk.clone(),
                image: args.image.clone(),
                user: args.user.clone(),
                dotfiles: args.dotfiles.clone(),
                idle: args.idle.clone(),
                no_agents: args.no_agents.clone(),
                routing: args.routing.map(|routing| match routing {
                    DevboxRouting::TraefikFile => Routing::TraefikFile,
                    DevboxRouting::None => Routing::None,
                }),
                traefik_file: args.traefik_file.clone(),
                github_token: args.github_token_from.is_some(),
                ssh_keys: args.ssh_keys.clone(),
                binaries: args.binaries.clone(),
                release: args.release.clone(),
                ..UpOptions::default()
            };
            let up = devbox::UpArgs {
                options,
                wake_password_file: args.wake_password_file.clone(),
                app_password_file: args.app_password_file.clone(),
                dry_run: args.dry_run,
                yes: args.yes,
            };
            devbox::up(&cx, &up, out)?
        }
        DevboxCommand::Down { destroy, dry_run, yes, keep_vm: _ } => {
            state.ensure(host.as_ref())?;
            devbox::down(&cx, *destroy, *dry_run, *yes, out)?
        }
        DevboxCommand::Status => devbox::status(host.as_ref(), json, out)?,
        DevboxCommand::Doctor => devbox::doctor(host.as_ref(), json, out)?,
        DevboxCommand::Connect => devbox::connect(host.as_ref(), out)?,
        DevboxCommand::Password { which, password_file } => {
            devbox::password(&cx, matches!(which, DevboxPassword::App), password_file.as_ref(), out)?
        }
    };
    // the health monitor: adopted by name, pointed at the health URL — shown, then done, because
    // running `moi devbox up` is asking for it
    if matches!(command, DevboxCommand::Up(args) if !args.dry_run)
        && loaded.config.monitors.kuma.enabled
        && loaded.config.devbox.monitor.is_some()
    {
        use crate::monitors::{self, Action, Options};
        let docker = crate::docker::Docker::new(Arc::clone(&host));
        let options = Options { update: true, prune: false };
        let mine = |plan: &monitors::Plan| -> Vec<monitors::Step> {
            plan.steps
                .iter()
                .filter(|s| {
                    s.owner.as_deref() == Some("devbox")
                        && matches!(s.action, Action::Adopt | Action::Create | Action::Update)
                })
                .cloned()
                .collect()
        };
        let db = state.open_db(host.as_ref())?;
        let planned = monitors::local_inventory(host.as_ref(), &loaded.config, &docker).and_then(|inventory| {
            monitors::plan_for(host.as_ref(), &loaded.config, &docker, &state, &db, &inventory, options)
        });
        let steps = planned.as_ref().map(mine).unwrap_or_default();
        for step in &steps {
            let _ = writeln!(out, "moi: monitor \"{}\": {} ({})", step.monitor, step.action.as_str(), step.detail);
        }
        if !steps.is_empty() {
            let applied = monitors::connect(&host, &loaded.config).and_then(|mut kuma| {
                let inventory = kuma.inventory().map_err(Error::Monitors)?;
                let plan =
                    monitors::plan_for(host.as_ref(), &loaded.config, &docker, &state, &db, &inventory, options)?;
                let plan = monitors::Plan { steps: mine(&plan), warnings: Vec::new() };
                monitors::apply::apply(&host, &loaded.config, &docker, &db, &mut kuma, &plan, &inventory)
            });
            let _ = match applied {
                Ok(report) if report.failed.is_empty() => writeln!(out, "moi: monitor done ({})", report.summary()),
                Ok(report) => writeln!(
                    out,
                    "moi: monitor not changed: {}",
                    report.failed.iter().map(|f| f.error.clone()).collect::<Vec<_>>().join("; ")
                ),
                Err(e) => writeln!(
                    out,
                    "moi: monitor not changed ({e}): `moi monitors sync --update` does it once Kuma can be reached"
                ),
            };
        }
    }
    if done.changed {
        // moi's records follow: devbox's identity among the backup paths, the snapshot, the context file
        for path in devbox::records::extend_backup_paths(host.as_ref(), &loaded.config, &loaded.paths) {
            let _ = writeln!(out, "moi: {path} added to backup.paths in {}", loaded.paths.config.display());
        }
        if let Ok(now) = crate::config::load(host.as_ref(), &cli.paths()) {
            let config = Arc::new(now.config);
            let recorded = state
                .open_db(host.as_ref())
                .and_then(|db| crate::snapshot::capture(&host, &config, &state, &db, &now.masker))
                .and_then(|_| crate::context::write(host.as_ref(), &config, &state, &now.masker));
            let _ = match recorded {
                Ok(_) => writeln!(out, "moi: snapshot and CONTEXT.md updated"),
                Err(e) => writeln!(out, "moi: the snapshot afterwards failed ({e}): run `moi snapshot`"),
            };
        }
    }
    if done.code == 0 { Ok(()) } else { Err(Error::ProblemsFound) }
}

fn ssh(cli: &Cli, command: &SshCommand, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::ssh::{self, apply};
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    state.ensure(host.as_ref())?;
    let db = state.open_db(host.as_ref())?;
    let cx = apply::Cx { host: &host, loaded: &loaded, state: &state, db: &db };
    let say = |out: &mut dyn Write, line: &str| {
        let _ = writeln!(out, "{}", loaded.masker.mask(line));
    };
    // after a change: the server's description follows it
    let record = |out: &mut dyn Write| {
        // the configuration as it is now (an `up` may just have written [ssh])
        let Ok(now) = crate::config::load(host.as_ref(), &cli.paths()) else { return };
        let config = Arc::new(now.config);
        match crate::snapshot::capture(&host, &config, &state, &db, &now.masker)
            .and_then(|_| crate::context::write(host.as_ref(), &config, &state, &now.masker))
        {
            Ok(_) => say(out, "snapshot and CONTEXT.md updated"),
            Err(e) => say(out, &format!("the snapshot afterwards failed ({e}): run `moi snapshot`")),
        }
    };
    match command {
        SshCommand::Up { hostname, dry_run, yes, inner } => {
            let settings = ssh::settings(host.as_ref(), &loaded, hostname.as_deref())?;
            let plan = ssh::provider(settings.provider).plan(&host, settings);
            // the unit behind the rollback timer: the tunnel steps only, into the record it was given
            if let Some(archive) = inner {
                if let Some(stop) = plan.stopped() {
                    return Err(Error::Usage(stop.text.clone()));
                }
                ssh::provider(plan.settings.provider).apply(&cx, &plan, archive).map_err(Error::Usage)?;
                return Ok(());
            }
            if cli.json && *dry_run {
                print_json(
                    out,
                    &json!({ "ok": plan.stopped().is_none(), "hostname": plan.settings.hostname, "changes": plan.changes(), "steps": plan.steps }),
                );
            } else {
                let _ = write!(out, "{}", loaded.masker.mask(&ssh::render(&plan)));
            }
            if plan.stopped().is_some() {
                return Err(Error::ProblemsFound);
            }
            if *dry_run || plan.changes() == 0 {
                return Ok(());
            }
            if !*yes {
                let answer = host
                    .ask("Apply this plan? [y/N] ", std::time::Duration::from_secs(600))
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                if !answer.trim().starts_with(['y', 'Y']) {
                    say(out, "nothing was changed");
                    return Ok(());
                }
            }
            let (outcome, archive) = apply::up(&cx, &plan)?;
            let (done, waiting) = match outcome {
                apply::Outcome::Applied(done) => (done, false),
                apply::Outcome::AwaitingConfirmation(done) => (done, true),
            };
            for line in &done {
                say(out, &format!("  done   {line}"));
            }
            for line in apply::write_records(&cx, &plan, &archive).map_err(Error::Usage)? {
                say(out, &format!("  done   {line}"));
            }
            record(out);
            // the proof: an SSH server answers behind the public name
            let verified = if plan.tunnel.is_some() {
                match apply::handshake(&host, &plan.settings.hostname) {
                    Ok(ms) => {
                        say(
                            out,
                            &format!("  ✓ an SSH server answers at https://{}/ ({ms} ms)", plan.settings.hostname),
                        );
                        true
                    }
                    Err(e) => {
                        say(
                            out,
                            &format!(
                                "  ✗ the public handshake failed ({}: {}). DNS may need a minute: `moi ssh status`; `moi ssh down` puts everything back",
                                e.stage, e.error
                            ),
                        );
                        false
                    }
                }
            } else {
                true
            };
            if waiting {
                say(
                    out,
                    &format!(
                        "Now open a NEW connection (`ssh {}`) and run `sudo moi ssh confirm` within {} minutes.",
                        plan.settings.alias,
                        apply::CONFIRM.as_secs() / 60
                    ),
                );
                say(out, "Without it, everything is put back as it was.");
            } else {
                say(out, "next: `moi ssh connect` prints the command for your computers");
            }
            if verified { Ok(()) } else { Err(Error::ProblemsFound) }
        }
        SshCommand::Status => {
            let settings = ssh::settings(host.as_ref(), &loaded, None)?;
            let status = ssh::status(&host, &settings, apply::pending(&db).is_some());
            if cli.json {
                print_json(out, &json!({ "ok": true, "status": status }));
            } else {
                let _ = write!(out, "{}", loaded.masker.mask(&ssh::render_status(&status)));
            }
            if status.up { Ok(()) } else { Err(Error::ProblemsFound) }
        }
        SshCommand::Connect => {
            let settings = ssh::settings(host.as_ref(), &loaded, None)?;
            let text = ssh::connect(host.as_ref(), &settings)?;
            if cli.json {
                print_json(
                    out,
                    &json!({ "ok": true, "command": text.split("\n\n").next().unwrap_or_default(), "alias": settings.alias, "hostname": settings.hostname }),
                );
            } else {
                let _ = write!(out, "{text}");
            }
            Ok(())
        }
        SshCommand::Authorize { key, name } => {
            let user = loaded
                .config
                .ssh
                .user
                .clone()
                .or_else(|| host.env("SUDO_USER"))
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "root".to_string());
            let done = ssh::authorize(host.as_ref(), &user, key, name.as_deref())?;
            let (added, label) = match &done {
                ssh::Authorized::Added(label) => (true, label),
                ssh::Authorized::AlreadyThere(label) => (false, label),
            };
            if cli.json {
                print_json(out, &json!({ "ok": true, "added": added, "user": user, "name": label }));
            } else if added {
                say(out, &format!("{label} may now log in as {user}"));
            } else {
                say(out, &format!("that key is already allowed to log in as {user}: nothing was changed"));
            }
            Ok(())
        }
        SshCommand::Confirm => {
            let said = apply::confirm(&cx)?;
            if cli.json {
                print_json(out, &json!({ "ok": true, "message": said }));
            } else {
                say(out, &said);
            }
            Ok(())
        }
        SshCommand::Down { dry_run, rollback } => {
            let found = match rollback {
                Some(dir) => apply::load(host.as_ref(), dir).map(|record| (dir.clone(), record)),
                None => apply::latest(host.as_ref(), &state),
            };
            let Some((archive, _)) = found else {
                say(out, "nothing to restore: `moi ssh up` has changed nothing on this server");
                return Ok(());
            };
            let token = loaded
                .config
                .ssh
                .api_token
                .clone()
                .or_else(|| crate::config::secret_named(host.as_ref(), &loaded.paths, ssh::API_TOKEN_SECRET));
            let said =
                ssh::provider(loaded.config.ssh.provider).down(&cx, &archive, token, *dry_run).map_err(Error::Usage)?;
            if cli.json {
                print_json(out, &json!({ "ok": true, "dry_run": dry_run, "restored": said }));
            } else {
                for line in &said {
                    say(out, &format!("  {line}"));
                }
                if said.is_empty() {
                    say(out, "nothing had been changed");
                }
            }
            if !*dry_run {
                record(out);
            }
            Ok(())
        }
    }
}

fn verify(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    use crate::restore::verify;
    let loaded = crate::config::load(host.as_ref(), &cli.paths())?;
    let state = StateDir::new(PathBuf::from(&loaded.config.state.dir));
    state.ensure(host.as_ref())?;
    let db = state.open_db(host.as_ref())?;
    let config = Arc::new(loaded.config);
    // with nothing recorded yet there is nothing to hold the machine to
    if !host.exists(&state.snapshot()) {
        crate::snapshot::capture(&host, &config, &state, &db, &loaded.masker)?;
    }
    let items = verify::verify(&host, &config, &state, &db)?;
    let failed = items.iter().filter(|i| !i.ok).count();
    if cli.json {
        print_json(out, &json!({ "ok": true, "failed": failed, "checks": items }));
    } else {
        let _ = write!(out, "{}", loaded.masker.mask(&verify::render(&items)));
    }
    if failed == 0 { Ok(()) } else { Err(Error::ProblemsFound) }
}
