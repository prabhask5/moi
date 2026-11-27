//! Handing an item over from the legacy setup to moi, taking it back, and — when everything has been
//! moi's for a while — removing what is left of the legacy setup.
//!
//! Nothing is deleted before it is in the archive (`<state>/archive/legacy/<its path>`), and until
//! `finish` nothing but the triggers is touched at all: a switch moves a cron file away and disables
//! a unit; scripts, state and unit files stay where they are, so a rollback is putting the cron file
//! back and enabling the unit again.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::compare;
use super::shadow;
use super::{Item, Kind, Plan};
use crate::config::Config;
use crate::docker::Docker;
use crate::error::Error;
use crate::host::{self, Cmd, FileKind, Host};
use crate::state::StateDir;
use crate::state::db::Db;

/// How long everything must have been moi's before `finish` removes the legacy files.
pub const STABLE: i64 = 7 * 86_400;

/// What a switch did to one legacy piece, for undoing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Undo {
    kind: String,
    path: String,
    /// A unit that was enabled / running before.
    enabled: bool,
    active: bool,
}

#[derive(Debug)]
pub struct Cx<'a> {
    pub host: &'a Arc<dyn Host>,
    pub config: &'a Config,
    pub state: &'a StateDir,
    pub db: &'a Db,
}

impl Cx<'_> {
    fn archive(&self, path: &str) -> PathBuf {
        self.state.archive().join("legacy").join(path.trim_start_matches('/'))
    }

    fn log(&self, line: &str) {
        let file = self.state.archive().join("migration.log");
        let _ = self.host.create_dir_all(&self.state.archive(), 0o700);
        let stamp = host::format_rfc3339(self.host.now());
        let _ = self.host.append(&file, format!("{stamp} {line}\n").as_bytes(), 0o600);
    }

    fn systemctl(&self, args: &[&str]) -> bool {
        self.host.run(&Cmd::new("systemctl").args(args.iter().copied())).is_ok_and(|o| o.status == 0)
    }

    fn now(&self) -> i64 {
        host::unix_seconds(self.host.now())
    }
}

/// Copy a file or a directory tree, keeping modes and owners.
fn copy(host: &dyn Host, from: &Path, to: &Path) -> std::io::Result<usize> {
    let meta = host.symlink_metadata(from)?;
    if let Some(parent) = to.parent() {
        host.create_dir_all(parent, 0o755)?;
    }
    match meta.kind {
        FileKind::Dir => {
            host.create_dir_all(to, meta.mode)?;
            let _ = host.set_owner(to, meta.uid, meta.gid);
            let mut copied = 0;
            for name in host.read_dir(from)? {
                copied += copy(host, &from.join(&name), &to.join(&name))?;
            }
            Ok(copied)
        }
        FileKind::Symlink => {
            let _ = host.remove_file(to);
            host.symlink(&host.read_link(from)?, to).map(|()| 1)
        }
        _ => {
            host.write(to, &host.read(from)?, meta.mode)?;
            let _ = host.set_owner(to, meta.uid, meta.gid);
            Ok(1)
        }
    }
}

fn remove(host: &dyn Host, path: &Path) -> std::io::Result<()> {
    match host.symlink_metadata(path)?.kind {
        FileKind::Dir => crate::init::remove_tree(host, path),
        _ => host.remove_file(path),
    }
}

fn size(host: &dyn Host, path: &Path) -> u64 {
    match host.symlink_metadata(path) {
        Ok(meta) if meta.kind == FileKind::Dir => {
            host.read_dir(path).unwrap_or_default().iter().map(|n| size(host, &path.join(n))).sum()
        }
        Ok(meta) => meta.size,
        Err(_) => 0,
    }
}

fn unit_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The checks whose messages are compared before they may be switched.
fn is_check(config: &Config, item: &str) -> bool {
    crate::checks::enabled(config).iter().any(|c| c.name() == item)
}

/// Hand items over to moi. Returns a line per item: what was done, or why it was not.
pub fn switch(cx: &Cx<'_>, plan: &Plan, only: &[String], force: bool) -> Result<(Vec<String>, bool), Error> {
    if !shadow::active(cx.db) {
        return Err(Error::Usage(
            "the migration has not begun: run `moi migrate shadow` first, let it run, then `moi migrate compare`"
                .to_string(),
        ));
    }
    for name in only {
        if !plan.items.iter().any(|i| &i.name == name) {
            let known: Vec<&str> = plan.items.iter().map(|i| i.name.as_str()).collect();
            return Err(Error::Usage(format!("no such item: {name} (the plan has: {})", known.join(", "))));
        }
    }
    let docker = Docker::new(Arc::clone(cx.host));
    let compared = compare::compare(cx.host.as_ref(), cx.config, cx.state, cx.db, &docker);
    let mut said = Vec::new();
    let mut refused = false;
    for item in plan.items.iter().filter(|i| only.is_empty() || only.contains(&i.name)) {
        if !shadow::held(cx.db, &item.name) {
            said.push(format!("{}: already switched", item.name));
            continue;
        }
        // a check takes over a monitor people read: only when it says what the script says
        if is_check(cx.config, &item.name) && !force {
            let clean = compared.iter().find(|c| c.check == item.name).is_some_and(compare::Compared::clean);
            if !clean {
                refused = true;
                said.push(format!(
                    "{}: NOT switched — its last {} shadow runs are not all the same as the legacy script's (`moi migrate compare`; `--force` to switch anyway)",
                    item.name,
                    compare::WINDOW
                ));
                continue;
            }
        }
        match switch_item(cx, item) {
            Ok(line) => said.push(line),
            Err(why) => {
                refused = true;
                said.push(format!("{}: NOT switched — {why}", item.name));
            }
        }
    }
    Ok((said, refused))
}

fn switch_item(cx: &Cx<'_>, item: &Item) -> Result<String, String> {
    let host = cx.host.as_ref();
    // 1. everything into the archive, and what state the triggers are in
    let mut undo = Vec::new();
    let mut archived = 0;
    for legacy in &item.legacy {
        let from = Path::new(&legacy.path);
        if host.symlink_metadata(from).is_err() {
            continue;
        }
        archived +=
            copy(host, from, &cx.archive(&legacy.path)).map_err(|e| format!("cannot archive {}: {e}", legacy.path))?;
        let (enabled, active) = if legacy.kind == Kind::Unit {
            let unit = unit_name(&legacy.path);
            (cx.systemctl(&["is-enabled", "--quiet", unit]), cx.systemctl(&["is-active", "--quiet", unit]))
        } else {
            (false, false)
        };
        undo.push(Undo { kind: legacy.kind.as_str().to_string(), path: legacy.path.clone(), enabled, active });
    }
    cx.db.set_setting(&format!("migrate.undo.{}", item.name), &serde_json::to_string(&undo).unwrap_or_default());
    cx.log(&format!("switch {}: {archived} files archived", item.name));

    // 2. the legacy triggers stop
    let mut stopped = Vec::new();
    for piece in &undo {
        match piece.kind.as_str() {
            "cron" => {
                if let Err(e) = host.remove_file(Path::new(&piece.path)) {
                    undo_item(cx, &item.name, &undo);
                    return Err(format!("cannot remove {}: {e}; what was stopped is running again", piece.path));
                }
                stopped.push(piece.path.clone());
            }
            "unit" if piece.enabled || piece.active => {
                let unit = unit_name(&piece.path);
                if !cx.systemctl(&["disable", "--now", unit]) {
                    undo_item(cx, &item.name, &undo);
                    return Err(format!("systemctl disable --now {unit} failed; what was stopped is running again"));
                }
                stopped.push(unit.to_string());
            }
            _ => {}
        }
    }
    cx.log(&format!("switch {}: stopped {}", item.name, stopped.join(" ")));

    // 3. moi takes over, from where the legacy piece stopped
    let fresh = shadow::import(host, cx.config, cx.db, std::slice::from_ref(item)).map_err(|e| e.to_string())?;
    let mut took = Vec::new();
    let owner = format!("check:{}", item.name);
    if let Some(feeder) = crate::monitors::feeders(host, cx.config, cx.state).into_iter().find(|f| f.owner == owner) {
        // the same monitor, by the token the script pushed to: nothing is created, nothing renamed
        let bound = crate::monitors::bindings(cx.db).map_err(|e| e.to_string())?.into_iter().any(|b| b.owner == owner);
        if !bound {
            let docker = Docker::new(Arc::clone(cx.host));
            let monitor = crate::monitors::inventory::locate(host, &docker)
                .and_then(|db| crate::monitors::inventory::read(host, &db))
                .and_then(|inventory| {
                    inventory.monitors.into_iter().find(|m| m.push_token.as_deref() == Some(feeder.token.as_str()))
                });
            let name = monitor.as_ref().map(|m| m.name.clone()).unwrap_or_default();
            cx.db
                .conn()
                .execute(
                    "INSERT INTO monitor_bindings (owner, monitor_name, kuma_id, push_token, created_by_moi, updated_at) VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                    (&owner, &name, monitor.as_ref().map(|m| m.id), &feeder.token, cx.now()),
                )
                .map_err(|e| e.to_string())?;
            cx.db.set_setting(&format!("migrate.bound.{}", item.name), "1");
        }
        took.push("reports to the same monitor with the same token".to_string());
    }
    if item.name == "firewall" {
        match crate::firewall::apply_guarded(cx.host, cx.config, cx.state, cx.db) {
            Ok(_) => took.push("rules applied by moi".to_string()),
            Err(e) => {
                undo_item(cx, &item.name, &undo);
                return Err(format!("moi could not apply the firewall ({e}); the legacy unit is back"));
            }
        }
    }
    if item.name == "serverstate" {
        took.extend(import_serverstate(cx, item));
    }
    took.extend(fresh);
    shadow::mark(cx.db, &item.name, true);
    cx.db.set_setting(&format!("migrate.switched_at.{}", item.name), &cx.now().to_string());
    cx.log(&format!("switch {}: done", item.name));
    let mut line = format!(
        "{}: switched — stopped {}",
        item.name,
        if stopped.is_empty() { "nothing (nothing was running)".to_string() } else { stopped.join(", ") }
    );
    for what in took {
        line.push_str(&format!("\n    {what}"));
    }
    Ok(line)
}

/// The prototype's history before moi's own, and its notes.
fn import_serverstate(cx: &Cx<'_>, item: &Item) -> Vec<String> {
    let host = cx.host.as_ref();
    let mut took = Vec::new();
    let Some(dir) = item.legacy.iter().find(|l| l.kind == Kind::State).map(|l| l.path.clone()) else { return took };
    if cx.db.setting("migrate.history_imported").is_none() && host.exists(&Path::new(&dir).join(".git")) {
        let imported = host
            .real_path(Path::new(&dir))
            .map_err(|e| Error::host(format!("cannot open {dir}"), e))
            .and_then(|real| crate::state::history::History::open(host, cx.state)?.import_before(&real));
        match imported {
            Ok(commits) => {
                cx.db.set_setting("migrate.history_imported", &commits.to_string());
                took.push(format!("{commits} commits of its history now come before moi's (`moi log`)"));
            }
            Err(e) => {
                took.push(format!("its history could NOT be imported ({e}); it stays in {dir} and in the archive"))
            }
        }
    }
    // notes: the owner's words are kept whichever file they are in
    if let Ok(old) = host.read_to_string(&Path::new(&dir).join("NOTES.md")) {
        let path = cx.state.notes();
        let mine = host.read_to_string(&path).unwrap_or_default();
        let marker = "## Notes from serverstate";
        if mine.trim().is_empty() {
            if host.write(&path, old.as_bytes(), 0o600).is_ok() {
                took.push("its NOTES.md is moi's now".to_string());
            }
        } else if !mine.contains(marker) && !mine.contains(old.trim()) {
            let merged = format!("{}\n\n{marker}\n\n{}\n", mine.trim_end(), old.trim());
            if host.write(&path, merged.as_bytes(), 0o600).is_ok() {
                took.push("its NOTES.md was added to moi's under \"Notes from serverstate\"".to_string());
            }
        }
    }
    took
}

/// Put the triggers of an item back as they were.
fn undo_item(cx: &Cx<'_>, name: &str, undo: &[Undo]) -> Vec<String> {
    let host = cx.host.as_ref();
    let mut back = Vec::new();
    for piece in undo {
        let path = Path::new(&piece.path);
        let archived = cx.archive(&piece.path);
        // a file that is gone or was changed comes back from the archive
        let differs = match (host.read(path), host.read(&archived)) {
            (Ok(now), Ok(then)) => now != then,
            (Err(_), Ok(_)) => true,
            _ => false,
        };
        if differs
            && host.symlink_metadata(&archived).is_ok_and(|m| m.kind != FileKind::Dir)
            && copy(host, &archived, path).is_ok()
        {
            back.push(piece.path.clone());
        }
        if piece.kind == "unit" {
            let unit = unit_name(&piece.path);
            let done = match (piece.enabled, piece.active) {
                (true, true) => cx.systemctl(&["enable", "--now", unit]),
                (true, false) => cx.systemctl(&["enable", unit]),
                (false, true) => cx.systemctl(&["start", unit]),
                (false, false) => false,
            };
            if done {
                back.push(unit.to_string());
            }
        }
    }
    cx.log(&format!("rollback {name}: {}", back.join(" ")));
    back
}

/// Give items back to the legacy setup. Returns a line per item.
pub fn rollback(cx: &Cx<'_>, plan: &Plan, only: &[String]) -> Result<Vec<String>, Error> {
    let mut said = Vec::new();
    let switched = shadow::switched(cx.db);
    for name in only {
        if !switched.contains(name) {
            return Err(Error::Usage(format!(
                "{name} is not switched (switched: {})",
                if switched.is_empty() { "nothing".to_string() } else { switched.join(", ") }
            )));
        }
    }
    // in the reverse order of the plan
    let order: Vec<&str> = plan.items.iter().map(|i| i.name.as_str()).rev().collect();
    let mut names: Vec<&String> = switched.iter().filter(|s| only.is_empty() || only.contains(s)).collect();
    names.sort_by_key(|n| order.iter().position(|o| o == n).unwrap_or(usize::MAX));
    for name in names {
        let undo: Vec<Undo> = cx
            .db
            .setting(&format!("migrate.undo.{name}"))
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        // moi lets go first: no moment with two voices
        shadow::mark(cx.db, name, false);
        if cx.db.setting(&format!("migrate.bound.{name}")).is_some() {
            let _ = cx.db.conn().execute("DELETE FROM monitor_bindings WHERE owner = ?1", [format!("check:{name}")]);
            cx.db.unset_setting(&format!("migrate.bound.{name}"));
        }
        cx.db.unset_setting(&format!("migrate.switched_at.{name}"));
        let back = undo_item(cx, name, &undo);
        said.push(format!(
            "{name}: rolled back — {}",
            if back.is_empty() { "nothing had been stopped".to_string() } else { format!("back: {}", back.join(", ")) }
        ));
    }
    if said.is_empty() {
        said.push("nothing is switched".to_string());
    }
    Ok(said)
}

/// Remove what is left of the legacy setup. Everything must be switched, and have been for a week
/// unless `now`.
pub fn finish(cx: &Cx<'_>, plan: &Plan, now: bool) -> Result<Vec<String>, Error> {
    let host = cx.host.as_ref();
    if !shadow::active(cx.db) {
        return Err(Error::Usage("no migration is under way (`moi migrate plan` shows what one would do)".to_string()));
    }
    let held: Vec<&str> = plan.items.iter().map(|i| i.name.as_str()).filter(|n| shadow::held(cx.db, n)).collect();
    if !held.is_empty() {
        return Err(Error::Usage(format!(
            "not everything is switched yet: {} (`moi migrate switch`)",
            held.join(", ")
        )));
    }
    let at = |name: &str| {
        cx.db.setting(&format!("migrate.switched_at.{name}")).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0)
    };
    let latest = plan.items.iter().map(|i| at(&i.name)).max().unwrap_or(0);
    let age = cx.now() - latest;
    if !now && age < STABLE {
        return Err(Error::Usage(format!(
            "the last item was switched {} days ago: the legacy files are removed after 7 days without a rollback (`--now` to remove them today)",
            age / 86_400
        )));
    }
    // the backup must have shown that it works in moi's hands: a broken one is not found out by deleting the old
    if plan.items.iter().any(|i| i.name == "backup") {
        let since = at("backup");
        let last: Option<String> = cx
            .db
            .conn()
            .query_row(
                "SELECT status FROM backup_runs WHERE at >= ?1 ORDER BY at DESC, id DESC LIMIT 1",
                [since],
                |r| r.get(0),
            )
            .ok();
        if last.as_deref() != Some("ok") {
            return Err(Error::Usage(match last {
                Some(_) => "moi's last backup since the switch FAILED: fix it (`moi backup`) before the legacy backup is removed".to_string(),
                None => "moi has not run a backup since the switch: wait for tonight's, or run `moi backup`, before the legacy backup is removed".to_string(),
            }));
        }
    }
    let mut said = Vec::new();
    let (mut removed, mut freed) = (0_usize, 0_u64);
    for item in &plan.items {
        // the prototype removes itself best: it knows what its installer put where
        if item.name == "serverstate" {
            let command = item.legacy.iter().map(|l| l.path.as_str()).find(|p| {
                p.ends_with("/serverstate") && host.metadata(Path::new(p)).is_ok_and(|m| m.kind == FileKind::File)
            });
            for legacy in &item.legacy {
                if host.symlink_metadata(Path::new(&legacy.path)).is_ok()
                    && host.symlink_metadata(&cx.archive(&legacy.path)).is_err()
                {
                    let _ = copy(host, Path::new(&legacy.path), &cx.archive(&legacy.path));
                }
            }
            if let Some(command) = command
                && host.run(&Cmd::new(command).args(["uninstall", "--purge", "--yes"])).is_ok_and(|o| o.status == 0)
            {
                said.push("serverstate: removed by its own uninstaller".to_string());
            }
        }
        for legacy in &item.legacy {
            let path = Path::new(&legacy.path);
            if host.symlink_metadata(path).is_err() {
                continue;
            }
            // never without a copy
            if host.symlink_metadata(&cx.archive(&legacy.path)).is_err()
                && copy(host, path, &cx.archive(&legacy.path)).is_err()
            {
                said.push(format!("{}: kept — it could not be archived", legacy.path));
                continue;
            }
            if legacy.kind == Kind::Unit {
                let _ = cx.systemctl(&["disable", "--now", unit_name(&legacy.path)]);
            }
            let bytes = size(host, path);
            match remove(host, path) {
                Ok(()) => {
                    removed += 1;
                    freed += bytes;
                }
                Err(e) => said.push(format!("{}: could not be removed ({e})", legacy.path)),
            }
        }
        cx.db.unset_setting(&format!("migrate.undo.{}", item.name));
        cx.db.unset_setting(&format!("migrate.bound.{}", item.name));
        cx.db.unset_setting(&format!("migrate.switched_at.{}", item.name));
    }
    let _ = cx.systemctl(&["daemon-reload"]);
    shadow::end(cx.db);
    cx.log(&format!("finish: {removed} legacy pieces removed, {freed} bytes"));
    said.insert(
        0,
        format!(
            "{removed} legacy files, units and directories removed ({} freed); copies are in {}",
            crate::audit::human(freed),
            cx.state.archive().join("legacy").display()
        ),
    );
    Ok(said)
}
