//! Backups. So far: the *identity set* — what a backup covers and why — which the audit and the context
//! file need to say what is and is not protected. Dumps, archives and restic arrive in M7.
//!
//! Spec: docs/spec/07-backup.md § What is backed up.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::config::{AutoOr, Config};
use crate::snapshot::View;

/// Host paths a container sees but that are the machine's own, never application data.
pub const HOST_SYSTEM_PATHS: [&str; 18] = [
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/etc/localtime",
    "/etc/timezone",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/var/run",
    "/tmp",
    "/etc/hostname",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/lib/modules",
    "/usr",
    "/etc/machine-id",
    "/var/log",
    "/etc/ssl",
];

/// True if `path` is `base` or inside it.
pub fn under(path: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    base.is_empty() || path == base || path.strip_prefix(base).is_some_and(|rest| rest.starts_with('/'))
}

/// A database moi can dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    /// Container or swarm service name.
    pub name: String,
    pub engine: &'static str,
    /// Host paths holding its raw data: useless as a file copy while it runs, so dumps replace them.
    pub data_dirs: Vec<String>,
    /// False when `backup.databases` is a list that leaves it out.
    pub dumped: bool,
}

/// How the backup treats a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// In the backup, and why.
    Covered(String),
    /// Declared fungible (with the owner's reason, if given): never backed up, never a gap.
    Fungible(String),
    /// Raw data of a database that is dumped instead.
    Dumped(String),
    /// Raw data of a database nothing dumps.
    UndumpedDatabase(String),
    NotCovered,
}

/// What `moi backup` covers on this server.
#[derive(Debug, Clone)]
pub struct Identity {
    pub enabled: bool,
    /// (path, why it is in the set), sorted, without duplicates.
    pub paths: Vec<(String, String)>,
    pub databases: Vec<Database>,
    fungible: Vec<(String, String)>,
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// Postgres or MySQL/MariaDB, recognised by the environment their images define or by the image's own
/// name (`postgres`, `pgvector`, `supabase/postgres` — but not `postgrest` or `postgres-meta`).
pub(crate) fn database_engine(image: &str, env: &Value) -> Option<&'static str> {
    let has = |key: &str| env.as_array().into_iter().flatten().filter_map(|e| e.as_str()).any(|e| e.starts_with(key));
    let lower = image.to_lowercase();
    let name = lower
        .split('@')
        .next()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    if has("PGDATA=")
        || has("POSTGRES_PASSWORD")
        || has("POSTGRES_USER=")
        || has("POSTGRES_DB=")
        || ["postgres", "pgvector", "postgis", "timescaledb"].contains(&name)
    {
        Some("postgres")
    } else if has("MARIADB_ROOT_PASSWORD") || has("MYSQL_ROOT_PASSWORD") || ["mariadb", "mysql"].contains(&name) {
        Some("mysql")
    } else {
        None
    }
}

fn database_dir(destination: &str) -> bool {
    ["/var/lib/postgresql", "/var/lib/mysql", "/var/lib/mariadb"].iter().any(|d| under(destination, d))
}

fn volume_path(name: &str) -> String {
    format!("/var/lib/docker/volumes/{name}/_data")
}

impl Identity {
    pub fn compute(config: &Config, view: &View<'_>) -> Self {
        let backup = &config.backup;
        let fungible: Vec<(String, String)> = config
            .data
            .fungible
            .iter()
            .map(|p| (p.clone(), config.data.fungible_reason.get(p).cloned().unwrap_or_default()))
            .collect();
        let is_fungible = |path: &str| fungible.iter().any(|(f, _)| under(path, f));

        // databases, from containers and swarm services
        let mut databases = Vec::new();
        let wanted = |name: &str| match &backup.databases {
            AutoOr::Auto => true,
            AutoOr::List(names) => names.iter().any(|n| n == name),
        };
        for c in view.containers() {
            if let Some(engine) = database_engine(text(&c["image"]), &c["env"]) {
                let data_dirs = c["mounts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| database_dir(text(&m["destination"])))
                    .map(|m| match text(&m["type"]) {
                        "volume" if !text(&m["name"]).is_empty() => volume_path(text(&m["name"])),
                        _ => text(&m["source"]).to_string(),
                    })
                    .filter(|p| !p.is_empty())
                    .collect();
                let name = text(&c["name"]).to_string();
                databases.push(Database { dumped: backup.enabled && wanted(&name), name, engine, data_dirs });
            }
        }
        for s in view.services() {
            if let Some(engine) = database_engine(text(&s["image"]), &s["env"]) {
                let data_dirs = s["mounts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| database_dir(text(&m["destination"])))
                    .map(|m| {
                        if text(&m["type"]) == "volume" {
                            volume_path(text(&m["source"]))
                        } else {
                            text(&m["source"]).to_string()
                        }
                    })
                    .collect();
                let name = text(&s["name"]).to_string();
                databases.push(Database { dumped: backup.enabled && wanted(&name), name, engine, data_dirs });
            }
        }
        let is_database_dir = |path: &str| databases.iter().any(|d| d.data_dirs.iter().any(|dir| under(path, dir)));

        let mut paths: BTreeSet<(String, String)> = BTreeSet::new();
        if backup.enabled {
            let mut add = |path: &str, why: String| {
                if !path.is_empty() && !is_fungible(path) && !is_database_dir(path) {
                    paths.insert((path.to_string(), why));
                }
            };
            for p in &backup.paths {
                add(p, "backup.paths".to_string());
            }
            for project in view.compose_projects() {
                add(text(&project["workdir"]), format!("compose stack {}", text(&project["project"])));
            }
            let volume_wanted = |name: &str| match &backup.volumes {
                AutoOr::Auto => true,
                AutoOr::List(names) => names.iter().any(|n| n == name),
            };
            for (owner, mounts, source_key) in
                view.containers().iter().map(|c| (text(&c["name"]).to_string(), c["mounts"].clone(), "name")).chain(
                    view.services()
                        .iter()
                        .map(|s| (format!("service {}", text(&s["name"])), s["mounts"].clone(), "source")),
                )
            {
                for m in mounts.as_array().into_iter().flatten() {
                    let source = text(&m["source"]);
                    match text(&m["type"]) {
                        "bind" if m["ro"] != true && !HOST_SYSTEM_PATHS.iter().any(|p| under(source, p)) => {
                            add(source, format!("data of {owner}"));
                        }
                        "volume" if !text(&m[source_key]).is_empty() && volume_wanted(text(&m[source_key])) => {
                            add(
                                &volume_path(text(&m[source_key])),
                                format!("volume {} of {owner}", text(&m[source_key])),
                            );
                        }
                        _ => {}
                    }
                }
            }
            add("/etc/moi", "moi's own configuration".to_string());
            add(&config.state.dir, "moi's state".to_string());
        }
        // one reason per path is enough to explain it
        let mut unique: Vec<(String, String)> = Vec::new();
        for (path, why) in paths {
            if unique.last().is_none_or(|(last, _)| *last != path) {
                unique.push((path, why));
            }
        }
        Self { enabled: backup.enabled, paths: unique, databases, fungible }
    }

    /// How the backup treats `path`.
    pub fn coverage(&self, path: &str) -> Coverage {
        if let Some((_, reason)) = self.fungible.iter().find(|(f, _)| under(path, f)) {
            return Coverage::Fungible(reason.clone());
        }
        if let Some(db) =
            self.databases.iter().find(|d| d.data_dirs.iter().any(|dir| under(path, dir) || under(dir, path)))
        {
            let own = db.data_dirs.iter().any(|dir| under(path, dir));
            if own {
                return if db.dumped {
                    Coverage::Dumped(db.name.clone())
                } else {
                    Coverage::UndumpedDatabase(db.name.clone())
                };
            }
        }
        match self.paths.iter().find(|(covered, _)| under(path, covered)) {
            Some((_, why)) => Coverage::Covered(why.clone()),
            None => Coverage::NotCovered,
        }
    }

    /// The fungible root a path is under, with its reason.
    pub fn fungible_root(&self, path: &str) -> Option<&(String, String)> {
        self.fungible.iter().find(|(f, _)| under(path, f))
    }

    /// Declared fungible paths with their reasons.
    pub fn fungible(&self) -> &[(String, String)] {
        &self.fungible
    }
}
