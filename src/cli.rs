//! Command-line interface: argument definitions and dispatch.
//!
//! `src/main.rs` only calls [`run`]. Every command exists from day one; unimplemented ones
//! return [`Error::NotImplemented`] (exit code 3) and name the milestone that delivers them.
//!
//! Output contract: results on stdout (human text, or one JSON document with `--json` that always has
//! an `ok` field), diagnostics and log lines on stderr, exit codes as in `error`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use serde_json::json;

use crate::config::Paths;
use crate::error::Error;
use crate::host::{FixtureHost, Host, RealHost};
use crate::log::{Level, Logger};

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
        /// Link lifetime (default from config, capped at context.share.max_ttl)
        #[arg(long)]
        ttl: Option<String>,
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
    for (name, value) in std::env::vars().filter(|(name, _)| name.starts_with("MOI_")) {
        host.set_env(&name, &value);
    }
    Ok(Arc::new(host))
}

fn dispatch(cli: &Cli, host: Arc<dyn Host>, out: &mut dyn Write) -> Result<(), Error> {
    let (what, milestone) = match &cli.command {
        Command::Config(cmd) => return config(cli, cmd, host, out),
        Command::Snapshot(_) | Command::Log { .. } | Command::Diff { .. } => ("snapshot & history", "M2"),
        Command::Context { .. } | Command::Notes | Command::Audit => ("context, notes & audit", "M3"),
        Command::Check(_) => ("health checks", "M4"),
        Command::Monitors(_) => ("Uptime Kuma monitors", "M5"),
        Command::Daemon | Command::Status | Command::Firewall(_) | Command::Init(_) | Command::Uninstall { .. } => {
            ("daemon, init & firewall", "M6")
        }
        Command::Backup(_) | Command::Push { .. } | Command::Pull { .. } | Command::RecoveryCard => {
            ("backup & state bundle", "M7")
        }
        Command::Restore(_) | Command::Verify => ("restore & verify", "M8"),
        Command::Migrate(_) => ("migration", "M9"),
    };
    Err(Error::NotImplemented { what, milestone })
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
