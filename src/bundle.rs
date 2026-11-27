//! The state bundle: everything needed to rebuild the server except its data — the state directory
//! (snapshot, history, notes) and `/etc/moi` — kept in the backup's restic repository as snapshots
//! tagged `moi-bundle` (docs/spec/08-bundle.md).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::backup::offsite::{BUNDLE_TAG, Restic};
use crate::config::Config;
use crate::error::Error;
use crate::host::{self, FileKind, Host};
use crate::state::StateDir;
use crate::state::db::Db;

const CONFIG_DIR: &str = "/etc/moi";

/// A digest of what the bundle would hold: the state's history head and every file of the
/// configuration. Changes exactly when a push would store something new.
fn fingerprint(host: &dyn Host, state: &StateDir) -> String {
    let mut hasher = Sha256::new();
    // the head of the state repository names the whole recorded state
    for file in ["HEAD", "refs/heads/main", "refs/heads/master"] {
        hasher.update(host.read(&state.state().join(".git").join(file)).unwrap_or_default());
    }
    hasher.update(host.read(&state.notes()).unwrap_or_default());
    let mut stack = vec![Path::new(CONFIG_DIR).to_path_buf()];
    while let Some(path) = stack.pop() {
        match host.symlink_metadata(&path).map(|m| m.kind) {
            Ok(FileKind::Dir) => {
                let mut names = host.read_dir(&path).unwrap_or_default();
                names.sort();
                stack.extend(names.into_iter().rev().map(|n| path.join(n)));
            }
            Ok(_) => {
                hasher.update(path.to_string_lossy().as_bytes());
                hasher.update(host.read(&path).unwrap_or_default());
            }
            Err(_) => {}
        }
    }
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// What a push did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    Saved,
    /// Nothing changed since the last push.
    Unchanged,
}

/// Store the bundle in the repository now. Without `force`, only if something changed since the last
/// push.
pub fn push(host: &Arc<dyn Host>, config: &Config, state: &StateDir, db: &Db, force: bool) -> Result<Pushed, Error> {
    let offsite = &config.backup.offsite;
    if !offsite.enabled {
        return Err(Error::Usage(
            "there is no off-site repository to push the state bundle to: add [backup.offsite] (repository, password) to moi.toml. Until then the state directory itself is the only copy".to_string(),
        ));
    }
    let now = fingerprint(host.as_ref(), state);
    let last: Option<String> =
        db.conn().query_row("SELECT value FROM settings WHERE key = 'bundle.pushed'", [], |r| r.get(0)).ok();
    if !force && last.as_deref() == Some(now.as_str()) {
        return Ok(Pushed::Unchanged);
    }
    let restic = Restic::new(host, offsite)?;
    let state_dir = state.state().to_string_lossy().into_owned();
    let failed = |what: &str, out: std::io::Result<host::CmdOutput>| match out {
        Ok(out) if out.status == 0 || out.status == 3 => Ok(()),
        Ok(out) => Err(Error::Usage(format!(
            "restic {what} failed ({}): {}",
            out.status,
            out.stderr_str().lines().last().unwrap_or_default().trim()
        ))),
        Err(e) => Err(Error::host("cannot run restic (is it installed?)", e)),
    };
    failed(
        "backup",
        restic.run(
            &[
                "backup",
                "--quiet",
                "--tag",
                BUNDLE_TAG,
                "--host",
                config.server.name.as_str(),
                state_dir.as_str(),
                CONFIG_DIR,
            ],
            None,
        ),
    )?;
    failed(
        "forget",
        restic.run(
            &[
                "forget",
                "--quiet",
                "--tag",
                BUNDLE_TAG,
                "--host",
                config.server.name.as_str(),
                "--keep-last",
                "30",
                "--keep-daily",
                "30",
            ],
            None,
        ),
    )?;
    let stamp = host::unix_seconds(host.now()).to_string();
    for (key, value) in [("bundle.pushed", now.as_str()), ("bundle.pushed_at", stamp.as_str())] {
        db.conn()
            .execute("INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", (key, value))
            .map_err(|e| Error::Db(e.to_string()))?;
    }
    Ok(Pushed::Saved)
}

/// What the daemon does after a change: push, unless nothing changed or the last push is less than
/// an hour old. Never an error — the next change, or the nightly backup, pushes again.
pub fn push_if_due(host: &Arc<dyn Host>, config: &Config, state: &StateDir, db: &Db) -> Option<Pushed> {
    if !config.backup.offsite.enabled {
        return None;
    }
    let last: Option<i64> = db
        .conn()
        .query_row("SELECT value FROM settings WHERE key = 'bundle.pushed_at'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok());
    if last.is_some_and(|at| host::unix_seconds(host.now()) - at < 3600) {
        return None;
    }
    push(host, config, state, db, false).ok()
}

/// What a pull brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    /// Where the bundle is now: `<dir>/var/lib/moi/state` and `<dir>/etc/moi`.
    pub dir: String,
    pub host: String,
    /// When the snapshot was taken (as restic prints it).
    pub time: String,
}

/// Restore the latest bundle of a repository into `<state root>/restore/<time>/`.
pub fn pull(host: &Arc<dyn Host>, restic: &Restic, root: &Path) -> Result<Pulled, Error> {
    let run = |args: &[&str]| {
        restic
            .run(args, Some(Duration::from_secs(1800)))
            .map_err(|e| Error::host("cannot run restic (install it first)", e))
    };
    let listed = run(&["snapshots", "--tag", BUNDLE_TAG, "--json"])?;
    if listed.status != 0 {
        return Err(Error::Usage(format!(
            "cannot read the repository: {}",
            listed.stderr_str().lines().last().unwrap_or("restic gave no reason").trim()
        )));
    }
    let snapshots: Vec<Value> = serde_json::from_slice(&listed.stdout).unwrap_or_default();
    // restic lists oldest first; the time is RFC 3339, which sorts as text
    let latest =
        snapshots.iter().max_by_key(|s| s["time"].as_str().unwrap_or_default().to_string()).ok_or_else(|| {
            Error::Usage(
                "this repository holds no state bundle (no snapshot tagged moi-bundle): is it the right repository?"
                    .to_string(),
            )
        })?;
    let id = latest["id"].as_str().unwrap_or_default();
    let stamp = host::format_rfc3339(host.now()).replace(':', "");
    let dir = root.join("restore").join(stamp);
    host.create_dir_all(&dir, 0o700).map_err(|e| Error::host(format!("cannot create {}", dir.display()), e))?;
    let target = dir.to_string_lossy().into_owned();
    let restored = run(&["restore", id, "--target", target.as_str()])?;
    if restored.status != 0 {
        return Err(Error::Usage(format!(
            "restic restore failed: {}",
            restored.stderr_str().lines().last().unwrap_or_default().trim()
        )));
    }
    Ok(Pulled {
        dir: target,
        host: latest["hostname"].as_str().unwrap_or_default().to_string(),
        time: latest["time"].as_str().unwrap_or_default().to_string(),
    })
}

/// What the owner must keep somewhere that is not this server. Names of secrets, never their values.
pub fn recovery_card(host: &dyn Host, config: &Config, config_path: &Path) -> String {
    // which secret holds what: the names are in moi.toml, the values only in secrets.toml
    let text = host.read_to_string(config_path).unwrap_or_default();
    let offsite_section = text.find("[backup.offsite]").map_or("", |start| &text[start..]);
    let secret_of = |key: &str| -> Option<String> {
        // `key = "secret:name"`, on a line of its own or inside an inline table
        let at = offsite_section.match_indices(key).find_map(|(i, _)| {
            let before_ok = i == 0
                || !offsite_section.as_bytes()[i - 1].is_ascii_alphanumeric()
                    && offsite_section.as_bytes()[i - 1] != b'_';
            let rest = offsite_section[i + key.len()..].trim_start();
            (before_ok && rest.starts_with('=')).then(|| rest[1..].trim_start())
        })?;
        Some(at.strip_prefix("\"secret:")?.split('"').next()?.to_string())
    };
    let named = |key: &str| {
        secret_of(key).map_or_else(
            || "(not configured)".to_string(),
            |name| format!("secret `{name}` in {}", config_path.with_file_name("secrets.toml").display()),
        )
    };
    let mut out = format!(
        "Recovery card for {}\n\nRebuilding this server on new hardware needs these, kept somewhere that is NOT this server\n(a password manager, a printed sheet). This card names them; it does not contain them.\n\n",
        config.server.name
    );
    let offsite = &config.backup.offsite;
    if offsite.enabled {
        out.push_str(&format!("[ ] restic repository URL      {}\n", named("repository")));
        out.push_str(&format!("[ ] restic repository password {}\n", named("password")));
        for key in offsite.env.keys() {
            out.push_str(&format!("[ ] {key:<26} {}\n", named(key)));
        }
        out.push_str(&format!(
            "    backups are tagged `{}`, the state bundle `moi-bundle`, host `{}`\n",
            offsite.tag, config.server.name
        ));
    } else {
        out.push_str("[!] No off-site backup is configured: if this machine is lost, so is everything on it.\n    Add [backup.offsite] to moi.toml.\n");
    }
    if config.monitors.kuma.enabled {
        out.push_str(&format!(
            "[ ] Uptime Kuma login          {} (Kuma's own data comes back with the backup)\n",
            config.monitors.kuma.url.clone().unwrap_or_default()
        ));
    }
    for path in &config.data.fungible {
        let reason = config
            .data
            .fungible_reason
            .get(path)
            .cloned()
            .unwrap_or_else(|| "declared replaceable, no note on how to get it back".to_string());
        out.push_str(&format!("[ ] not backed up: {path} — {reason}\n"));
    }
    out.push_str("\nOn the new machine:\n  1. install moi (and restic)\n  2. export RESTIC_PASSWORD and the repository's access keys, then: sudo -E moi pull --repository <URL>\n  3. sudo moi restore --bundle /var/lib/moi/restore/<time> --dry-run\n  4. sudo moi restore --bundle /var/lib/moi/restore/<time>\n");
    out
}
