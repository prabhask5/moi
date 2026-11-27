//! What a collector is, what it produces, and the helpers collectors share.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::docker::Docker;
use crate::host::{Cmd, FileKind, Host};

/// Everything a collector may use. Collectors never write: they read through `host` and `docker`.
#[derive(Debug)]
pub struct Ctx {
    pub host: Arc<dyn Host>,
    pub config: Arc<Config>,
    pub docker: Docker,
}

/// One area of the server (docs/spec/02-snapshot.md § Collectors).
pub trait Collector: Send + Sync {
    /// Short name: used in reports, logs and `collector-errors.json`.
    fn name(&self) -> &'static str;

    /// Read the area. An `Err` (or taking too long) keeps this collector's previous output in place.
    fn collect(&self, cx: &Ctx) -> Result<Output, String>;
}

/// What a collector wants stored.
#[derive(Debug, Default)]
pub struct Output {
    /// Generated files under `snapshot/`: (relative path, content).
    pub(crate) stable: Vec<(String, Vec<u8>)>,
    /// Generated files under `volatile/`.
    pub(crate) volatile: Vec<(String, Vec<u8>)>,
    /// Host files and symlinks to copy verbatim under `snapshot/files/`.
    pub(crate) files: Vec<PathBuf>,
}

impl Output {
    /// A generated file in the stable tree. Empty content means "no such file".
    pub fn stable(&mut self, path: &str, content: impl Into<Vec<u8>>) {
        let content = content.into();
        if !content.is_empty() {
            self.stable.push((path.to_string(), content));
        }
    }

    /// A generated file in the volatile tree. Empty content means "no such file".
    pub fn volatile(&mut self, path: &str, content: impl Into<Vec<u8>>) {
        let content = content.into();
        if !content.is_empty() {
            self.volatile.push((path.to_string(), content));
        }
    }

    /// Copy this host file or symlink (missing ones are skipped when the snapshot is written).
    pub fn grab(&mut self, path: impl Into<PathBuf>) {
        self.files.push(path.into());
    }

    pub fn grab_all(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.files.extend(paths);
    }
}

/// Lines joined with newlines, with a final newline; empty for no lines.
pub(crate) fn lines<S: AsRef<str>>(items: impl IntoIterator<Item = S>) -> String {
    let mut out = String::new();
    for item in items {
        out.push_str(item.as_ref());
        out.push('\n');
    }
    out
}

/// Run an optional external tool. `None` if it is not installed, fails to start, times out or exits
/// non-zero — every tool moi shells out to is feature-detected this way.
pub fn run_tool(host: &dyn Host, program: &str, args: &[&str]) -> Option<String> {
    let cmd = Cmd::new(program).args(args.iter().copied()).timeout(Duration::from_secs(20));
    let out = host.run(&cmd).ok()?;
    out.success().then(|| out.stdout_str().into_owned())
}

/// How to walk a directory tree for files to capture.
#[derive(Debug, Clone)]
pub struct Walk {
    /// Directory levels below the root to descend into (1 = the root's own entries only).
    pub max_depth: usize,
    /// Directory names never entered.
    pub prune_names: Vec<String>,
    /// Absolute paths never entered or captured.
    pub skip_paths: Vec<String>,
    /// File-name suffixes never captured (`.log`, `.db`, `-wal`, …).
    pub skip_suffixes: Vec<String>,
    /// Stop after this many files.
    pub limit: usize,
}

impl Default for Walk {
    fn default() -> Self {
        Self {
            max_depth: 64,
            prune_names: Vec::new(),
            skip_paths: Vec::new(),
            skip_suffixes: Vec::new(),
            limit: usize::MAX,
        }
    }
}

/// Directories that never hold configuration, whatever tree they are found in.
pub(crate) const LIGHT_PRUNE: [&str; 7] =
    [".git", "node_modules", "__pycache__", ".cache", "venv", ".venv", "site-packages"];

/// Data files that are never configuration: databases, logs, media, runtime state.
pub(crate) const DATA_SUFFIXES: [&str; 11] =
    [".sqlite", ".sqlite3", ".db", ".log", ".pid", "-wal", "-shm", ".mkv", ".mp4", ".iso", ".db-journal"];

/// Files and symlinks under `root` (or `root` itself if it is one), sorted. Symlinks are listed, never
/// followed, so a walk cannot leave the tree it was given.
pub fn walk_files(host: &dyn Host, root: &Path, walk: &Walk) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    let skipped = |p: &Path| walk.skip_paths.iter().any(|s| p == Path::new(s));
    match host.symlink_metadata(root).map(|m| m.kind) {
        Ok(FileKind::Dir) => {}
        Ok(FileKind::File | FileKind::Symlink) => return vec![root.to_path_buf()],
        _ => return Vec::new(),
    }
    let mut stack = vec![(root.to_path_buf(), 1usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(names) = host.read_dir(&dir) else { continue };
        for name in names {
            if out.len() >= walk.limit {
                return out.into_iter().collect();
            }
            let path = dir.join(&name);
            if skipped(&path) {
                continue;
            }
            match host.symlink_metadata(&path).map(|m| m.kind) {
                Ok(FileKind::Dir) => {
                    if depth < walk.max_depth && !walk.prune_names.contains(&name) {
                        stack.push((path, depth + 1));
                    }
                }
                Ok(FileKind::File | FileKind::Symlink) => {
                    if !walk.skip_suffixes.iter().any(|s| name.ends_with(s.as_str())) {
                        out.insert(path);
                    }
                }
                _ => {}
            }
        }
    }
    out.into_iter().collect()
}

/// Expand `*` in the last-but-any path component of a pattern like `/home/*/bin` (no `**`, no `?`).
pub(crate) fn expand_glob(host: &dyn Host, pattern: &str) -> Vec<PathBuf> {
    let mut found = vec![PathBuf::from("/")];
    for part in pattern.split('/').filter(|p| !p.is_empty()) {
        let mut next = Vec::new();
        for base in &found {
            if part.contains('*') {
                for name in host.read_dir(base).unwrap_or_default() {
                    if wildcard(part, &name) {
                        next.push(base.join(name));
                    }
                }
            } else {
                next.push(base.join(part));
            }
        }
        found = next;
    }
    found.retain(|p| host.exists(p));
    found.sort();
    found
}

/// `*` matches any run of characters.
pub(crate) fn wildcard(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else { return pattern == name };
    let Some(mut rest) = name.strip_prefix(first) else { return false };
    let mut parts: Vec<&str> = parts.collect();
    let Some(last) = parts.pop() else { return rest.is_empty() };
    for part in parts {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// True if the bytes look like text (no NUL in the first 8 KiB).
pub(crate) fn is_text(data: &[u8]) -> bool {
    !data.iter().take(8192).any(|b| *b == 0)
}
