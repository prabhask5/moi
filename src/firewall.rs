//! The host firewall: moi owns exactly the chains named in `firewall.chains` and fills them from the
//! rules files — nothing else in the kernel's tables is touched (docs/spec/10-daemon.md § Firewall).
//!
//! A chain is replaced in one `iptables-restore --noflush` transaction: the generated input flushes
//! the owned chains and adds their rules, so there is no moment with half a rule set, and Docker's own
//! chains and every other table stay as they are.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::Error;
use crate::host::{self, Cmd, Host};
use crate::state::StateDir;
use crate::state::db::Db;

/// Chains the kernel has from the start; every other owned chain is created if missing.
const BUILTIN: [&str; 3] = ["INPUT", "FORWARD", "OUTPUT"];
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
/// Saved rule sets kept for rolling back.
const KEEP_SAVED: usize = 10;

/// IPv4 or IPv6: its tools and its rules file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    V4,
    V6,
}

impl Family {
    pub const ALL: [Family; 2] = [Family::V4, Family::V6];

    pub fn name(self) -> &'static str {
        match self {
            Family::V4 => "v4",
            Family::V6 => "v6",
        }
    }

    fn tool(self) -> &'static str {
        match self {
            Family::V4 => "iptables",
            Family::V6 => "ip6tables",
        }
    }

    fn rules_file(self, config: &Config) -> &str {
        match self {
            Family::V4 => &config.firewall.rules_v4,
            Family::V6 => &config.firewall.rules_v6,
        }
    }
}

/// Whether a chain exists without Docker having to create it.
pub fn builtin(chain: &str) -> bool {
    BUILTIN.contains(&chain)
}

fn usage(message: String) -> Error {
    Error::Usage(message)
}

/// The rules of a family's file, checked: every line must fill one of the owned chains.
pub fn rules(host: &dyn Host, config: &Config, family: Family) -> Result<Vec<String>, Error> {
    let file = family.rules_file(config);
    let text = host
        .read_to_string(Path::new(file))
        .map_err(|e| Error::host(format!("cannot read the firewall rules {file}"), e))?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let (action, chain) = (words.next().unwrap_or_default(), words.next().unwrap_or_default());
        if !matches!(action, "-A" | "-I" | "-P") {
            return Err(usage(format!(
                "{file}:{}: only -A, -I and -P lines belong in a rules file, found `{line}`",
                n + 1
            )));
        }
        if !config.firewall.chains.iter().any(|owned| owned == chain) {
            return Err(usage(format!(
                "{file}:{}: chain {chain} is not one moi manages (firewall.chains = {:?})",
                n + 1,
                config.firewall.chains
            )));
        }
        if action == "-P" && !builtin(chain) {
            return Err(usage(format!("{file}:{}: only built-in chains have a policy", n + 1)));
        }
        out.push(line.to_string());
    }
    Ok(out)
}

/// What `iptables-restore --noflush` gets: flush these chains, then fill them — one transaction.
pub fn restore_input(chains: &[String], rules: &[String]) -> String {
    let mut out = String::from("*filter\n");
    // declaring a chain creates it if it is missing (and empties it); built-in ones are flushed below
    for chain in chains.iter().filter(|c| !builtin(c)) {
        out.push_str(&format!(":{chain} - [0:0]\n"));
    }
    for chain in chains.iter().filter(|c| builtin(c)) {
        out.push_str(&format!("-F {chain}\n"));
    }
    for rule in rules.iter().filter(|rule| chains.iter().any(|c| rule.split_whitespace().nth(1) == Some(c.as_str()))) {
        out.push_str(rule);
        out.push('\n');
    }
    out.push_str("COMMIT\n");
    out
}

/// The owned chains as the kernel has them now (`iptables -S <chain>`); a chain that does not exist
/// contributes nothing. `None` when the tool is not installed.
pub fn live(host: &dyn Host, config: &Config, family: Family) -> Option<String> {
    let mut out = String::new();
    for chain in &config.firewall.chains {
        match host.run(&Cmd::new(family.tool()).args(["-S", chain.as_str()]).timeout(TOOL_TIMEOUT)) {
            Ok(listed) if listed.status == 0 => out.push_str(&listed.stdout_str()),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    Some(out)
}

fn setting(db: &Db, key: &str) -> Option<String> {
    db.conn().query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).ok()
}

fn set(db: &Db, key: &str, value: &str) -> Result<(), Error> {
    db.conn()
        .execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )
        .map(|_| ())
        .map_err(|e| Error::Db(e.to_string()))
}

fn digest(rules: &[String]) -> String {
    Sha256::digest(rules.join("\n").as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Save a family's whole rule set (every table) before it is changed; returns where.
pub fn save(host: &dyn Host, state: &StateDir, family: Family) -> Result<String, Error> {
    let saved = host
        .run(&Cmd::new(format!("{}-save", family.tool())).timeout(TOOL_TIMEOUT))
        .map_err(|e| Error::host(format!("cannot run {}-save", family.tool()), e))?;
    if saved.status != 0 {
        return Err(usage(format!("{}-save failed: {}", family.tool(), saved.stderr_str().trim())));
    }
    let dir = state.archive().join("firewall");
    host.create_dir_all(&dir, 0o700).map_err(|e| Error::host(format!("cannot create {}", dir.display()), e))?;
    let stamp = host::format_rfc3339(host.now()).replace(':', "");
    let path = dir.join(format!("{stamp}.{}.rules", family.name()));
    host.write(&path, &saved.stdout, 0o600).map_err(|e| Error::host(format!("cannot write {}", path.display()), e))?;
    // older saves go; the newest few stay
    let suffix = format!(".{}.rules", family.name());
    let mut saves: Vec<String> =
        host.read_dir(&dir).unwrap_or_default().into_iter().filter(|name| name.ends_with(&suffix)).collect();
    saves.sort();
    for old in saves.iter().rev().skip(KEEP_SAVED) {
        let _ = host.remove_file(&dir.join(old));
    }
    Ok(path.to_string_lossy().into_owned())
}

/// Put a saved rule set back (every table, as it was).
pub fn restore(host: &dyn Host, family: Family, saved: &str) -> Result<(), Error> {
    let tool = format!("{}-restore", family.tool());
    let out = host
        .run(&Cmd::new(&tool).arg(saved).timeout(TOOL_TIMEOUT))
        .map_err(|e| Error::host(format!("cannot run {tool}"), e))?;
    if out.status == 0 { Ok(()) } else { Err(usage(format!("{tool} {saved} failed: {}", out.stderr_str().trim()))) }
}

/// What applying did for one family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Applied {
    pub family: Family,
    pub chains: Vec<String>,
    pub rules: usize,
}

/// Replace these owned chains of a family with the rules from its file.
pub fn apply(host: &dyn Host, config: &Config, db: &Db, family: Family, chains: &[String]) -> Result<Applied, Error> {
    let rules = rules(host, config, family)?;
    let input = restore_input(chains, &rules);
    let tool = format!("{}-restore", family.tool());
    let out = host
        .run(&Cmd::new(&tool).arg("--noflush").stdin(input.into_bytes()).timeout(TOOL_TIMEOUT))
        .map_err(|e| Error::host(format!("cannot run {tool} (is iptables installed?)"), e))?;
    if out.status != 0 {
        // the transaction failed as a whole: nothing was changed
        return Err(usage(format!(
            "{tool} refused the rules in {} (nothing was changed): {}",
            family.rules_file(config),
            out.stderr_str().trim()
        )));
    }
    // remember what was applied and what the kernel made of it, to notice later when either changes
    set(db, &format!("firewall.{}.file", family.name()), &digest(&rules))?;
    if chains.len() == config.firewall.chains.len() {
        set(db, &format!("firewall.{}.live", family.name()), &live(host, config, family).unwrap_or_default())?;
    }
    let applied =
        rules.iter().filter(|rule| chains.iter().any(|c| rule.split_whitespace().nth(1) == Some(c.as_str()))).count();
    Ok(Applied { family, chains: chains.to_vec(), rules: applied })
}

/// How long someone applying over SSH has to say that they can still reach the machine.
pub const CONFIRM: Duration = Duration::from_secs(60);
/// The transient unit that puts the previous rules back if nobody confirms.
pub const ROLLBACK_UNIT: &str = "moi-firewall-rollback";

/// What a guarded apply did, and where the rules it replaced are.
#[derive(Debug, Clone)]
pub struct Guarded {
    pub applied: Vec<Applied>,
    pub saved: Vec<(Family, String)>,
}

/// Apply both families with the way back arranged first: the rules in place are saved, and over SSH —
/// where a wrong rule locks the person out, and their shell with this very process — a systemd timer
/// outside the session puts them back unless the person confirms that they can still reach the machine.
pub fn apply_guarded(
    host: &std::sync::Arc<dyn Host>,
    config: &Config,
    state: &StateDir,
    db: &Db,
) -> Result<Guarded, Error> {
    // read both files first: a typo in one must not leave the other family applied alone
    for family in Family::ALL {
        rules(host.as_ref(), config, family)?;
    }
    let mut saved = Vec::new();
    for family in Family::ALL {
        saved.push((family, save(host.as_ref(), state, family)?));
    }
    let stop_timer = || host.run(&Cmd::new("systemctl").args(["stop", &format!("{ROLLBACK_UNIT}.timer")]));
    let remote = host.env("SSH_CONNECTION").is_some() || host.env("SSH_TTY").is_some();
    if remote {
        let back: Vec<String> = saved
            .iter()
            .map(|(family, file)| {
                format!("{}-restore '{file}'", if *family == Family::V4 { "iptables" } else { "ip6tables" })
            })
            .collect();
        let seconds = format!("--on-active={}s", CONFIRM.as_secs() + 30);
        let unit = format!("--unit={ROLLBACK_UNIT}");
        let script = back.join("; ");
        let timer = Cmd::new("systemd-run").args([
            "--quiet",
            "--collect",
            unit.as_str(),
            seconds.as_str(),
            "/bin/sh",
            "-c",
            script.as_str(),
        ]);
        let scheduled =
            host.run(&timer).map_err(|e| Error::host("cannot schedule the automatic rollback (systemd-run)", e))?;
        if scheduled.status != 0 {
            return Err(Error::Usage(format!(
                "not applying over SSH without a way back: systemd-run could not schedule the rollback ({}). Run this at the console instead",
                scheduled.stderr_str().trim()
            )));
        }
    }
    let mut applied = Vec::new();
    for family in Family::ALL {
        match apply(host.as_ref(), config, db, family, &config.firewall.chains) {
            Ok(done) => applied.push(done),
            Err(e) => {
                // one family must not stay applied alone: put back what was there
                for (family, file) in &saved {
                    let _ = restore(host.as_ref(), *family, file);
                }
                if remote {
                    let _ = stop_timer();
                }
                return Err(e);
            }
        }
    }
    if remote {
        let prompt = format!(
            "Rules applied. If you can still read this, type yes within {} seconds to keep them; otherwise they are rolled back: ",
            CONFIRM.as_secs()
        );
        let kept =
            host.ask(&prompt, CONFIRM).ok().flatten().is_some_and(|answer| answer.trim().eq_ignore_ascii_case("yes"));
        if !kept {
            // not confirmed: put everything back now rather than leave it to the timer
            for (family, file) in &saved {
                restore(host.as_ref(), *family, file)?;
            }
            let _ = stop_timer();
            return Err(Error::Usage("not confirmed: the previous rules are back in place".to_string()));
        }
        if !stop_timer().is_ok_and(|o| o.status == 0) {
            return Err(Error::Usage(format!(
                "the rules are applied, but the rollback timer could not be stopped: run `systemctl stop {ROLLBACK_UNIT}.timer` now, or they are rolled back in a minute"
            )));
        }
    }
    Ok(Guarded { applied, saved })
}

/// Where a family's owned chains stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The kernel has what moi applied, and the file has not changed since.
    InSync,
    /// moi has never applied this family.
    NeverApplied,
    /// The rules file changed since it was applied.
    Pending,
    /// Something else changed the owned chains since moi applied them.
    Drifted,
    /// The tool is not installed.
    Unavailable,
}

impl State {
    pub fn describe(self) -> &'static str {
        match self {
            State::InSync => "in sync",
            State::NeverApplied => "never applied by moi",
            State::Pending => "rules file changed since it was applied (moi firewall apply)",
            State::Drifted => "live rules differ from what moi applied (moi firewall diff)",
            State::Unavailable => "not available (tool not installed)",
        }
    }
}

pub fn state(host: &dyn Host, config: &Config, db: &Db, family: Family) -> Result<State, Error> {
    let Some(now) = live(host, config, family) else { return Ok(State::Unavailable) };
    let (Some(file), Some(applied)) = (
        setting(db, &format!("firewall.{}.file", family.name())),
        setting(db, &format!("firewall.{}.live", family.name())),
    ) else {
        return Ok(State::NeverApplied);
    };
    if digest(&rules(host, config, family)?) != file {
        return Ok(State::Pending);
    }
    Ok(if now == applied { State::InSync } else { State::Drifted })
}

/// What differs, as two unified diffs: the kernel against what moi last applied (someone changed the
/// chains), and against that the rules file as it is now (to be applied).
pub fn diff(host: &dyn Host, config: &Config, db: &Db, family: Family) -> Result<String, Error> {
    let mut out = String::new();
    let now = live(host, config, family).unwrap_or_default();
    let name = family.name();
    match setting(db, &format!("firewall.{name}.live")) {
        Some(applied) => {
            if applied != now {
                out.push_str(&format!(
                    "--- {name}: applied by moi\n+++ {name}: live\n{}",
                    crate::state::diff::unified(&applied, &now, 3)
                ));
            }
            let wanted = rules(host, config, family)?;
            if Some(digest(&wanted)) != setting(db, &format!("firewall.{name}.file")) {
                out.push_str(&format!(
                    "{name}: {} changed since it was applied; `moi firewall apply` replaces the chains with:\n",
                    family.rules_file(config)
                ));
                for rule in wanted {
                    out.push_str(&format!("  {rule}\n"));
                }
            }
        }
        None => {
            // nothing to compare with yet: show both sides
            let wanted = rules(host, config, family)?.join("\n") + "\n";
            out.push_str(&format!(
                "--- {name}: live\n+++ {name}: {}\n{}",
                family.rules_file(config),
                crate::state::diff::unified(&now, &wanted, 3)
            ));
        }
    }
    Ok(out)
}
