//! Running in the shadow of the legacy setup: which items moi holds back from, and what it takes
//! over from the legacy scripts' memory so that its first real run continues theirs.
//!
//! While an item is *held*, the legacy piece still does the work and moi only watches: a held check
//! runs and is recorded but not pushed, a held heal rule does not act, a held backup uploads nothing,
//! the held firewall and boot stacks are left to their units. `moi migrate switch` releases an item.

use std::collections::BTreeMap;
use std::path::Path;

use super::{Item, Kind};
use crate::checks::Memory;
use crate::config::{Config, Paths, When};
use crate::error::Error;
use crate::host::{self, Host};
use crate::state::db::Db;

const ACTIVE: &str = "migrate.active";
const ITEMS: &str = "migrate.items";
const SWITCHED: &str = "migrate.switched";

fn list(db: &Db, key: &str) -> Vec<String> {
    db.setting(key).and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

/// Whether a migration is under way on this server.
pub fn active(db: &Db) -> bool {
    db.setting(ACTIVE).is_some()
}

/// The items of the migration and the ones already handed over to moi.
pub fn items(db: &Db) -> Vec<String> {
    list(db, ITEMS)
}

pub fn switched(db: &Db) -> Vec<String> {
    list(db, SWITCHED)
}

/// True while the legacy setup still does this item's work: moi watches and does not act.
pub fn held(db: &Db, item: &str) -> bool {
    active(db) && items(db).iter().any(|i| i == item) && !switched(db).iter().any(|i| i == item)
}

/// Begin (or continue) the migration with these items. What was switched stays switched.
pub fn start(db: &Db, plan: &[Item]) {
    let names: Vec<&str> = plan.iter().map(|i| i.name.as_str()).collect();
    db.set_setting(ITEMS, &serde_json::to_string(&names).unwrap_or_default());
    db.set_setting(ACTIVE, "1");
}

/// Note that an item is moi's now — or the legacy setup's again.
pub fn mark(db: &Db, item: &str, is_switched: bool) {
    let mut all = switched(db);
    all.retain(|i| i != item);
    if is_switched {
        all.push(item.to_string());
    }
    all.sort();
    db.set_setting(SWITCHED, &serde_json::to_string(&all).unwrap_or_default());
}

/// The migration is over: nothing is held any more.
pub fn end(db: &Db) {
    for key in [ACTIVE, ITEMS, SWITCHED] {
        db.unset_setting(key);
    }
}

/// Take over what the legacy scripts remembered, so that deltas and cooldowns go on from where they
/// are: the formats of the reference scripts (`reference/cestmoi/scripts`), found in the state
/// directories the plan names. Returns what was taken; anything in another format is left where it is.
pub fn import(host: &dyn Host, config: &Config, db: &Db, plan: &[Item]) -> Result<Vec<String>, Error> {
    let mut done = Vec::new();
    let now = host::unix_seconds(host.now());
    let read = |path: &str| host.read_to_string(Path::new(path)).ok().map(|s| s.trim().to_string());
    let states = |name: &str| -> Vec<String> {
        plan.iter()
            .filter(|i| i.name == name)
            .flat_map(|i| &i.legacy)
            .filter(|l| l.kind == Kind::State)
            .map(|l| l.path.clone())
            .collect()
    };

    // resources: CPU counters of the last run, restart counts, conditions already seen once
    for dir in states("resources") {
        let mut memory = Memory::load(db, "resources")?;
        let mut took = Vec::new();
        if let Some(cpu) = read(&format!("{dir}/cpu"))
            .filter(|v| v.split_whitespace().filter(|n| n.parse::<u64>().is_ok()).count() == 2)
        {
            memory.values.insert("cpu".to_string(), cpu);
            took.push("CPU counters".to_string());
        }
        if let Some(restarts) = read(&format!("{dir}/restarts")) {
            let counts: BTreeMap<String, u64> = restarts
                .lines()
                .filter_map(|l| {
                    let (name, count) = l.rsplit_once(' ')?;
                    Some((name.to_string(), count.parse().ok()?))
                })
                .collect();
            if !counts.is_empty() {
                took.push(format!("restart counts of {} containers", counts.len()));
                memory.values.insert("restarts".to_string(), serde_json::to_string(&counts).unwrap_or_default());
            }
        }
        for name in host.read_dir(Path::new(&dir)).unwrap_or_default() {
            if let Some(condition) = name.strip_prefix("hot-") {
                let runs = memory.sustained.entry(condition.to_string()).or_insert(0);
                *runs = (*runs).max(1);
                took.push(format!("{condition} already high once"));
            }
        }
        if !took.is_empty() {
            memory.save(db, "resources", now)?;
            done.push(format!("resources: {} (from {dir})", took.join(", ")));
        }
    }

    // services: when the legacy script last healed, for every rule the check fires that has a cooldown
    for stamp in states("services") {
        let Some(last) = read(&stamp).and_then(|v| v.parse::<i64>().ok()) else { continue };
        for rule in config.heal.iter().filter(|r| matches!(r.when, When::Failing(_)) && r.cooldown.is_some()) {
            let known: Option<i64> = db
                .conn()
                .query_row("SELECT last_run FROM heal_cooldowns WHERE rule = ?1", [&rule.name], |r| r.get(0))
                .ok();
            if known.is_none_or(|k| k < last) {
                db.conn()
                    .execute(
                        "INSERT INTO heal_cooldowns (rule, last_run) VALUES (?1, ?2) ON CONFLICT(rule) DO UPDATE SET last_run = excluded.last_run",
                        (&rule.name, last),
                    )
                    .map_err(|e| Error::Db(e.to_string()))?;
                done.push(format!("heal rule {}: last acted {} (from {stamp})", rule.name, when(last)));
            }
        }
    }

    // backup: when the last upload was, so that the rate limit holds across the switch
    for dir in states("backup") {
        let Some(last) = read(&format!("{dir}/last-upload")).and_then(|v| v.parse::<i64>().ok()) else { continue };
        let known = db.setting("offsite.last_upload").and_then(|v| v.parse::<i64>().ok());
        if known.is_none_or(|k| k < last) {
            db.set_setting("offsite.last_upload", &last.to_string());
            done.push(format!("backup: last upload {} (from {dir}/last-upload)", when(last)));
        }
    }
    Ok(done)
}

fn when(unix: i64) -> String {
    host::format_rfc3339(std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(unix).unwrap_or(0)))
}

/// `NAME=value` pairs of a shell script or an env file (several on a line, quoted or not).
fn assignments(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches("export ");
        if line.starts_with('#') {
            continue;
        }
        for part in line.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else { continue };
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
                continue;
            }
            let value = value.trim();
            let value = match value.chars().next() {
                Some(quote @ ('"' | '\'')) => value[1..].split(quote).next().unwrap_or_default(),
                _ => value.split_whitespace().next().unwrap_or_default(),
            };
            if !value.is_empty() && !value.contains('$') {
                out.entry(name.to_string()).or_insert_with(|| value.to_string());
            }
        }
    }
    out
}

/// Fill secrets that `moi.toml` refers to and `secrets.toml` lacks from where the legacy setup kept
/// them: variables in its scripts and the env files those name. Works on the raw files, because a
/// configuration with a missing secret does not load. Returns the names that were added; values that
/// exist are never changed.
pub fn import_secrets(host: &dyn Host, paths: &Paths) -> Result<Vec<String>, Error> {
    let raw = host
        .read_to_string(&paths.config)
        .map_err(|e| Error::host(format!("cannot read {}", paths.config.display()), e))?;
    let Ok(config) = raw.parse::<toml::Table>() else { return Ok(Vec::new()) };
    let secrets_text = host.read_to_string(&paths.secrets).unwrap_or_default();
    let have: toml::Table = secrets_text
        .parse::<toml::Table>()
        .ok()
        .and_then(|t| t.get("secrets").and_then(toml::Value::as_table).cloned())
        .unwrap_or_default();

    // what the legacy setup knows: variables of its scripts, and of the env files they point to
    let dirs: Vec<String> = config
        .get("capture")
        .and_then(|c| c.get("script_dirs"))
        .and_then(toml::Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_else(|| {
            ["/usr/local/bin", "/usr/local/sbin", "/opt/scripts", "/root/bin"].map(String::from).to_vec()
        });
    let mut known: BTreeMap<String, String> = BTreeMap::new();
    for dir in &dirs {
        for name in host.read_dir(Path::new(dir)).unwrap_or_default() {
            let path = Path::new(dir).join(&name);
            // scripts are small; a binary is not a script
            if host.metadata(&path).is_ok_and(|m| m.kind == host::FileKind::File && m.size < 1_000_000) {
                let Ok(text) = host.read_to_string(&path) else { continue };
                let found = assignments(&text);
                for file in found.values().filter(|v| v.starts_with('/') && v.ends_with(".env")) {
                    for (key, value) in assignments(&host.read_to_string(Path::new(file)).unwrap_or_default()) {
                        known.entry(key).or_insert(value);
                    }
                }
                for (key, value) in found {
                    known.entry(key).or_insert(value);
                }
            }
        }
    }
    if let Some(file) = known.get("RESTIC_PASSWORD_FILE").cloned()
        && let Ok(password) = host.read_to_string(Path::new(&file))
    {
        known.entry("RESTIC_PASSWORD".to_string()).or_insert_with(|| password.trim().to_string());
    }

    // where moi.toml refers to a secret, and the legacy name of the same thing
    let reference = |value: Option<&toml::Value>| {
        value.and_then(toml::Value::as_str).and_then(|v| v.strip_prefix("secret:")).map(str::to_string)
    };
    let offsite = config.get("backup").and_then(|b| b.get("offsite"));
    let mut wanted: Vec<(String, String)> = Vec::new();
    let mut want = |name: Option<String>, legacy: &str| {
        if let Some(name) = name {
            wanted.push((name, legacy.to_string()));
        }
    };
    want(reference(offsite.and_then(|o| o.get("repository"))), "RESTIC_REPOSITORY");
    want(reference(offsite.and_then(|o| o.get("password"))), "RESTIC_PASSWORD");
    for (key, value) in offsite.and_then(|o| o.get("env")).and_then(toml::Value::as_table).into_iter().flatten() {
        want(reference(Some(value)), key);
    }
    let guard = offsite.and_then(|o| o.get("r2_guard"));
    want(reference(guard.and_then(|g| g.get("account_id"))), "CF_ACCOUNT_ID");
    want(reference(guard.and_then(|g| g.get("api_token"))), "CF_API_TOKEN");
    let probes = config
        .get("checks")
        .and_then(|c| c.get("services"))
        .and_then(|s| s.get("http"))
        .and_then(toml::Value::as_array);
    for probe in probes.into_iter().flatten() {
        if let Some(name) = reference(probe.get("api_key")) {
            let legacy = name.to_uppercase();
            want(Some(name), &legacy);
        }
    }

    let mut added: Vec<(String, String)> = Vec::new();
    for (name, legacy) in wanted {
        if have.contains_key(&name) || added.iter().any(|(n, _)| *n == name) {
            continue;
        }
        if let Some(value) = known.get(&legacy) {
            added.push((name, value.clone()));
        }
    }
    if added.is_empty() {
        return Ok(Vec::new());
    }
    let mut text = secrets_text;
    if !text.lines().any(|l| l.trim() == "[secrets]") {
        text.push_str("[secrets]\n");
    } else if !text.ends_with('\n') {
        text.push('\n');
    }
    // appended at the end: `[secrets]` is the only table the file has
    for (name, value) in &added {
        text.push_str(&format!("{name} = {}\n", toml::Value::String(value.clone())));
    }
    if let Some(parent) = paths.secrets.parent() {
        host.create_dir_all(parent, 0o755)
            .map_err(|e| Error::host(format!("cannot create {}", parent.display()), e))?;
    }
    host.write(&paths.secrets, text.as_bytes(), 0o600)
        .map_err(|e| Error::host(format!("cannot write {}", paths.secrets.display()), e))?;
    Ok(added.into_iter().map(|(name, _)| name).collect())
}
