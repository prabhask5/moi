//! What a backup run will do, worked out from the machine as it is now: which databases are dumped
//! and as whom, which paths are archived, and what is left out and why.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::{Identity, under};
use crate::config::Config;
use crate::docker::Docker;
use crate::host::{FileKind, Host, Metadata};
use crate::snapshot::{View, wildcard};

/// A database dump to take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dump {
    /// Container or swarm service name: the dump file is `<name>.sql.gz`.
    pub name: String,
    pub engine: &'static str,
    /// The running container to dump from (for a service: one of its tasks). `None`: not running.
    pub container: Option<String>,
    pub user: String,
    /// Environment for the dump command (the password). Never shown.
    #[serde(skip)]
    pub env: Vec<String>,
}

/// A path that goes into the archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Root {
    pub path: String,
    pub why: String,
    pub bytes: u64,
    pub files: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub dumps: Vec<Dump>,
    pub roots: Vec<Root>,
    /// (path or pattern, why it is left out)
    pub excluded: Vec<(String, String)>,
    /// Paths in the set that do not exist on this machine.
    pub missing: Vec<String>,
}

fn env_value(env: &Value, key: &str) -> Option<String> {
    env.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|e| e.strip_prefix(&format!("{key}=")).map(str::to_string))
}

/// Decides what a walk leaves out.
#[derive(Debug, Clone)]
pub struct Excluder {
    /// (path prefix, why)
    prefixes: Vec<(String, String)>,
    patterns: Vec<String>,
}

impl Excluder {
    pub fn new(config: &Config, identity: &Identity) -> Self {
        let mut prefixes: Vec<(String, String)> = Vec::new();
        for (path, reason) in identity.fungible() {
            prefixes.push((
                path.clone(),
                if reason.is_empty() { "fungible".to_string() } else { format!("fungible: {reason}") },
            ));
        }
        for db in identity.databases.iter().filter(|d| d.dumped) {
            for dir in &db.data_dirs {
                prefixes.push((dir.clone(), format!("raw files of {} (dumped instead)", db.name)));
            }
        }
        // never the backups themselves, nor what a run stages
        if let Some(local) = &config.backup.local_dir {
            prefixes.push((local.clone(), "the local backups themselves".to_string()));
        }
        prefixes.push((
            format!("{}/staging", config.state.dir.trim_end_matches('/')),
            "staging of the run itself".to_string(),
        ));
        let mut patterns = Vec::new();
        for pattern in &config.backup.exclude {
            if pattern.contains('*') {
                patterns.push(pattern.clone());
            } else {
                prefixes.push((pattern.clone(), "backup.exclude".to_string()));
            }
        }
        Self { prefixes, patterns }
    }

    /// Why a path is left out, if it is.
    pub fn why(&self, path: &str) -> Option<String> {
        if let Some((_, why)) = self.prefixes.iter().find(|(prefix, _)| under(path, prefix)) {
            return Some(why.clone());
        }
        self.patterns.iter().find(|pattern| wildcard(pattern, path)).map(|pattern| format!("backup.exclude {pattern}"))
    }

    /// Raw database directories, for the off-site step to skip too.
    pub fn raw_database_dirs(identity: &Identity) -> Vec<String> {
        let mut dirs: Vec<String> =
            identity.databases.iter().filter(|d| d.dumped).flat_map(|d| d.data_dirs.clone()).collect();
        dirs.sort();
        dirs.dedup();
        dirs
    }
}

/// A directory tagged as a cache (https://bford.info/cachedir/): rebuilt by its application.
fn cache_dir(host: &dyn Host, dir: &Path) -> bool {
    host.open(&dir.join("CACHEDIR.TAG")).is_ok_and(|mut tag| {
        let mut start = [0u8; 43];
        std::io::Read::read_exact(&mut tag, &mut start).is_ok()
            && &start == b"Signature: 8a477f597d28d172789f06886806bc55"
    })
}

/// Visit everything under `root` that a backup takes, depth first, directories before what is in them.
/// Symlinks are visited, never followed.
pub fn walk(host: &dyn Host, root: &Path, excluder: &Excluder, visit: &mut dyn FnMut(&Path, &Metadata)) {
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if excluder.why(&path.to_string_lossy()).is_some() {
            continue;
        }
        let Ok(meta) = host.symlink_metadata(&path) else { continue };
        if meta.kind == FileKind::Dir {
            if cache_dir(host, &path) {
                continue;
            }
            visit(&path, &meta);
            let mut names = host.read_dir(&path).unwrap_or_default();
            // the stack reverses: push in reverse to visit in name order
            names.sort();
            for name in names.into_iter().rev() {
                stack.push(path.join(name));
            }
        } else {
            visit(&path, &meta);
        }
    }
}

impl Plan {
    /// Work the plan out. Sizes are measured (the walk a run would do), so this takes as long as
    /// reading every directory entry in the set.
    pub fn make(host: &dyn Host, config: &Config, docker: &Docker, view: &View<'_>) -> (Self, Identity) {
        let identity = Identity::compute(config, view);
        let excluder = Excluder::new(config, &identity);
        let mut plan = Self::default();
        // ---- databases: who to connect as, and where
        let containers = docker.containers_inspect().unwrap_or_default();
        for db in identity.databases.iter().filter(|d| d.dumped) {
            let running = |c: &&Value| c["State"]["Running"].as_bool() == Some(true);
            let named = containers
                .iter()
                .filter(running)
                .find(|c| c["Name"].as_str().is_some_and(|n| n.trim_start_matches('/') == db.name));
            let task = containers
                .iter()
                .filter(running)
                .find(|c| c["Config"]["Labels"]["com.docker.swarm.service.name"] == db.name.as_str());
            let container = named.or(task);
            let env = container.map_or(&Value::Null, |c| &c["Config"]["Env"]);
            let image = container.and_then(|c| c["Config"]["Image"].as_str()).unwrap_or_default();
            let (user, dump_env) = if db.engine == "postgres" {
                let special = config
                    .backup
                    .special_users
                    .iter()
                    .find(|(substring, _)| image.contains(substring.as_str()))
                    .map(|(_, user)| user.clone());
                let user =
                    special.or_else(|| env_value(env, "POSTGRES_USER")).unwrap_or_else(|| "postgres".to_string());
                (user, env_value(env, "POSTGRES_PASSWORD").map(|pw| format!("PGPASSWORD={pw}")).into_iter().collect())
            } else {
                let password =
                    env_value(env, "MARIADB_ROOT_PASSWORD").or_else(|| env_value(env, "MYSQL_ROOT_PASSWORD"));
                ("root".to_string(), password.map(|pw| format!("MYSQL_PWD={pw}")).into_iter().collect())
            };
            plan.dumps.push(Dump {
                name: db.name.clone(),
                engine: db.engine,
                container: container.and_then(|c| c["Id"].as_str()).map(str::to_string),
                user,
                env: dump_env,
            });
        }
        // ---- paths: the outermost of nested ones, measured
        let all: Vec<&(String, String)> = identity.paths.iter().collect();
        for (path, why) in &identity.paths {
            if all.iter().any(|(other, _)| other != path && under(path, other)) {
                continue;
            }
            if let Some(reason) = excluder.why(path) {
                plan.excluded.push((path.clone(), reason));
                continue;
            }
            if !host.exists(Path::new(path)) {
                plan.missing.push(path.clone());
                continue;
            }
            let (mut bytes, mut files) = (0u64, 0u64);
            walk(host, Path::new(path), &excluder, &mut |_, meta| {
                if meta.kind == FileKind::File {
                    bytes += meta.size;
                    files += 1;
                }
            });
            plan.roots.push(Root { path: path.clone(), why: why.clone(), bytes, files });
        }
        // ---- what is left out, said once each
        for (path, reason) in identity.fungible() {
            plan.excluded.push((
                path.clone(),
                if reason.is_empty() { "fungible".to_string() } else { format!("fungible: {reason}") },
            ));
        }
        for db in identity.databases.iter().filter(|d| d.dumped) {
            for dir in &db.data_dirs {
                plan.excluded.push((dir.clone(), format!("raw files of {} (dumped instead)", db.name)));
            }
        }
        for pattern in &config.backup.exclude {
            plan.excluded.push((pattern.clone(), "backup.exclude".to_string()));
        }
        plan.excluded.sort();
        plan.excluded.dedup();
        (plan, identity)
    }
}
