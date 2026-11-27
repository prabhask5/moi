//! What the daemon watches, and when a burst of changes becomes one snapshot.

use std::path::{Path, PathBuf};

use crate::config::{Config, When};
use crate::docker::{Docker, Event};
use crate::host::{FileKind, Host};

/// A snapshot is taken when nothing has changed for this long …
pub const QUIET: i64 = 120;
/// … or this long after the first change, if changes never stop.
pub const MAX_WAIT: i64 = 900;
/// Directories under `/etc` are not watched beyond this many (a runaway tree must not eat the
/// kernel's watch limit).
const MAX_DIRS: usize = 4096;

/// Turns a stream of changes into moments to act.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Debounce {
    first: Option<i64>,
    last: Option<i64>,
}

impl Debounce {
    pub fn note(&mut self, now: i64) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// When to act on what was noted, if anything was.
    pub fn due_at(&self) -> Option<i64> {
        Some((self.last? + QUIET).min(self.first? + MAX_WAIT))
    }

    /// Start over (what was noted is being acted on); returns whether there was anything.
    pub fn take(&mut self) -> bool {
        self.last.take();
        self.first.take().is_some()
    }
}

/// Files that change without the server's setup changing: editor droppings, lock and cache files.
pub fn noise(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    name.ends_with('~')
        || name.starts_with(".#")
        || name.starts_with('.')
            && (name.ends_with(".swp")
                || name.ends_with(".swx")
                || name.ends_with(".lock")
                || name.contains(".moi-tmp."))
        || [".dpkg-new", ".dpkg-tmp", ".dpkg-old", ".tmp", ".bak"].iter().any(|suffix| name.ends_with(suffix))
        || name == "4913"
        || ["/etc/mtab", "/etc/adjtime", "/etc/ld.so.cache", "/etc/resolv.conf", "/etc/.pwd.lock"]
            .iter()
            .any(|churner| path == Path::new(churner))
}

/// Engine events that mean the setup changed. Most container events (exec, health, attach, top) do
/// not — and moi's own exec calls must not make it take snapshots of itself.
pub fn matters(event: &Event) -> bool {
    match event.kind.as_str() {
        "container" => matches!(event.action.as_str(), "create" | "destroy" | "start" | "die" | "rename" | "update"),
        "network" | "volume" => matches!(event.action.as_str(), "create" | "destroy" | "remove"),
        "service" => matches!(event.action.as_str(), "create" | "update" | "remove"),
        _ => false,
    }
}

/// Every directory to watch: `/etc` with everything below it, the crontab spool, the script
/// directories, the directories of compose projects, and where files that trigger heal rules live.
pub fn dirs(host: &dyn Host, config: &Config, docker: &Docker) -> Vec<PathBuf> {
    let excluded = |dir: &Path| {
        config.capture.etc_exclude.iter().any(|x| dir.starts_with(x))
            || config.capture.prune_dirs.iter().any(|p| dir.file_name().is_some_and(|n| n == p.as_str()))
    };
    let mut out: Vec<PathBuf> = Vec::new();
    // /etc, breadth first, never through symlinks
    let mut queue = std::collections::VecDeque::from([PathBuf::from("/etc")]);
    while let Some(dir) = queue.pop_front() {
        if out.len() >= MAX_DIRS {
            break;
        }
        for name in host.read_dir(&dir).unwrap_or_default() {
            let child = dir.join(name);
            if host.symlink_metadata(&child).is_ok_and(|m| m.kind == FileKind::Dir) && !excluded(&child) {
                queue.push_back(child);
            }
        }
        out.push(dir);
    }
    out.extend(["/var/spool/cron", "/var/spool/cron/crontabs"].map(PathBuf::from));
    out.extend(config.capture.script_dirs.iter().map(PathBuf::from));
    for container in docker.containers(true).unwrap_or_default() {
        if let Some(dir) = container["Labels"]["com.docker.compose.project.working_dir"].as_str() {
            out.push(PathBuf::from(dir));
        }
    }
    for rule in &config.heal {
        if let When::PathChanged(path) = &rule.when
            && let Some(parent) = Path::new(path).parent()
        {
            out.push(parent.to_path_buf());
        }
    }
    out.retain(|dir| host.metadata(dir).is_ok_and(|m| m.kind == FileKind::Dir));
    out.sort();
    out.dedup();
    out
}
