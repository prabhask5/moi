//! The state dir as a git repository: one commit per snapshot that changed something, written with
//! gitoxide (no `git` binary).
//!
//! Spec: docs/spec/02-snapshot.md § History. Only `snapshot/`, `NOTES.md` and `.gitignore` are committed.
//! Trees are built straight from the files (no index), so the repository never depends on a working
//! tree state that a crash could leave half-updated. The repository is opened *isolated*: neither the
//! machine's nor any user's git configuration is read.
//!
//! gitoxide needs a real directory, like SQLite needs a real file: `Host::real_path` is the bridge.

use std::collections::BTreeMap;
use std::path::Path;

use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use serde::Serialize;
use serde_json::Value;

use super::StateDir;
use crate::error::Error;
use crate::host::{self, FileKind, Host};

const GITIGNORE: &str = "volatile/\nCONTEXT.md\naudit.md\n";
const BRANCH: &str = "refs/heads/main";
/// Name-status lines listed in a commit body.
const MAX_STATUS_LINES: usize = 60;
/// Commits kept in `volatile/changelog.txt` for the context file.
const CHANGELOG_COMMITS: usize = 25;

pub struct History {
    repo: gix::Repository,
}

impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "History({})", self.repo.git_dir().display())
    }
}

/// One commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Full commit id.
    pub id: String,
    /// RFC 3339, UTC.
    pub time: String,
    pub subject: String,
    pub body: String,
}

/// How a path changed between two commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Status {
    #[serde(rename = "A")]
    Added,
    #[serde(rename = "M")]
    Modified,
    #[serde(rename = "D")]
    Deleted,
}

impl Status {
    pub fn letter(self) -> char {
        match self {
            Status::Added => 'A',
            Status::Modified => 'M',
            Status::Deleted => 'D',
        }
    }
}

/// One changed file of a commit, with both versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub status: Status,
    /// Relative to the state dir: `snapshot/docker/containers.json`.
    pub path: String,
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

fn git(e: impl std::fmt::Display) -> Error {
    Error::History(e.to_string())
}

/// Path → (kind, blob id) of every file in a tree.
type Files = BTreeMap<String, (EntryKind, gix::ObjectId)>;

impl History {
    /// Open the repository in the state dir, creating it on first use.
    pub fn open(host: &dyn Host, state: &StateDir) -> Result<Self, Error> {
        let dir = state.state();
        let real =
            host.real_path(&dir).map_err(|e| Error::host(format!("cannot open history in {}", dir.display()), e))?;
        // isolated from the machine's git configuration; the identity is for reflog lines only
        let options =
            gix::open::Options::isolated().config_overrides(["committer.name=moi", "committer.email=moi@localhost"]);
        let repo = if real.join(".git").is_dir() {
            gix::ThreadSafeRepository::open_opts(&real, options).map_err(git)?
        } else {
            gix::ThreadSafeRepository::init_opts(
                &real,
                gix::create::Kind::WithWorktree,
                gix::create::Options::default(),
                options,
            )
            .map_err(git)?
        };
        let ignore = dir.join(".gitignore");
        if host.read_to_string(&ignore).ok().as_deref() != Some(GITIGNORE) {
            host.write(&ignore, GITIGNORE.as_bytes(), 0o600).map_err(|e| Error::host("cannot write .gitignore", e))?;
        }
        Ok(Self { repo: repo.to_thread_local() })
    }

    fn head(&self) -> Option<gix::Commit<'_>> {
        self.repo.head_commit().ok()
    }

    /// Commit the current `snapshot/`, `NOTES.md` and `.gitignore` if they differ from the last commit.
    /// Returns the new commit, or `None` when nothing changed.
    pub fn commit(&self, host: &dyn Host, state: &StateDir) -> Result<Option<Entry>, Error> {
        let root = state.state();
        let mut files: Vec<String> = crate::snapshot::list_tree(host, &root, "snapshot");
        files.extend([".gitignore", "NOTES.md"].iter().filter(|f| host.exists(&root.join(f))).map(ToString::to_string));

        let mut editor = self.repo.edit_tree(gix::ObjectId::empty_tree(self.repo.object_hash())).map_err(git)?;
        for rel in &files {
            let path = root.join(rel);
            let (kind, content) = match host.symlink_metadata(&path).map(|m| m.kind) {
                Ok(FileKind::Symlink) => {
                    let target =
                        host.read_link(&path).map_err(|e| Error::host(format!("cannot read {}", path.display()), e))?;
                    (EntryKind::Link, target.to_string_lossy().into_owned().into_bytes())
                }
                Ok(FileKind::File) => (
                    EntryKind::Blob,
                    host.read(&path).map_err(|e| Error::host(format!("cannot read {}", path.display()), e))?,
                ),
                _ => continue,
            };
            let blob = self.repo.write_blob(&content).map_err(git)?;
            editor.upsert(rel.as_str(), kind, blob).map_err(git)?;
        }
        let tree = editor.write().map_err(git)?.detach();

        let head = self.head();
        if head.as_ref().and_then(|c| c.tree_id().ok()).is_some_and(|id| id.detach() == tree) {
            return Ok(None);
        }
        let message = match &head {
            None => format!("initial snapshot of {}", host::short_hostname(host)),
            Some(parent) => {
                let old = parent.tree_id().map_err(git)?.detach();
                let changes = self.changes_between(Some(old), tree)?;
                format!("changed: {}\n\n{}", subject(&changes), body(&changes))
            }
        };
        let name = "moi";
        let email = format!("moi@{}", host::short_hostname(host));
        let time = format!("{} +0000", host::unix_seconds(host.now()));
        let signature = gix::actor::SignatureRef { name: name.into(), email: email.as_str().into(), time: &time };
        let parents: Vec<gix::ObjectId> = head.iter().map(|c| c.id).collect();
        let id = self.repo.commit_as(signature, signature, BRANCH, message.trim_end(), tree, parents).map_err(git)?;
        self.entry(id.detach()).map(Some)
    }

    fn entry(&self, id: gix::ObjectId) -> Result<Entry, Error> {
        let commit = self.repo.find_commit(id).map_err(git)?;
        let message = commit.message().map_err(git)?;
        let seconds = commit.time().map_err(git)?.seconds;
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(seconds).unwrap_or(0));
        Ok(Entry {
            id: id.to_string(),
            time: host::format_rfc3339(time),
            subject: message.title.to_str_lossy().trim().to_string(),
            body: message.body.map(|b| b.to_str_lossy().trim_end().to_string()).unwrap_or_default(),
        })
    }

    /// The most recent `limit` commits, newest first.
    pub fn log(&self, limit: usize) -> Result<Vec<Entry>, Error> {
        let Some(head) = self.head() else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for info in self.repo.rev_walk([head.id]).first_parent_only().all().map_err(git)?.take(limit) {
            out.push(self.entry(info.map_err(git)?.id)?);
        }
        Ok(out)
    }

    /// Put the history of another state repository (the serverstate prototype's) *before* this one's:
    /// its commits come first, with their dates, authors and messages, and this repository's own
    /// commits follow on top of them. Only `snapshot/` and `NOTES.md` of each old commit are kept — the
    /// layout the two share. Returns how many commits were imported.
    pub fn import_before(&self, source: &Path) -> Result<usize, Error> {
        let options = gix::open::Options::isolated();
        let other = gix::ThreadSafeRepository::open_opts(source, options).map_err(git)?.to_thread_local();
        let Ok(tip) = other.head_commit() else { return Ok(0) };
        let mut old: Vec<gix::ObjectId> = Vec::new();
        for info in other.rev_walk([tip.id]).first_parent_only().all().map_err(git)? {
            old.push(info.map_err(git)?.id);
        }
        old.reverse();
        let mut own: Vec<gix::ObjectId> = Vec::new();
        if let Some(head) = self.head() {
            for info in self.repo.rev_walk([head.id]).first_parent_only().all().map_err(git)? {
                own.push(info.map_err(git)?.id);
            }
        }
        own.reverse();

        // every file content is copied once, however many commits have it
        let mut copied: BTreeMap<gix::ObjectId, gix::ObjectId> = BTreeMap::new();
        let mut parent: Option<gix::ObjectId> = None;
        let write = |repo: &gix::Repository,
                     from: &gix::Commit<'_>,
                     tree: gix::ObjectId,
                     parent: Option<gix::ObjectId>|
         -> Result<gix::ObjectId, Error> {
            let decoded = from.decode().map_err(git)?;
            let commit = gix::objs::Commit {
                tree,
                parents: parent.into_iter().collect(),
                author: decoded.author().map_err(git)?.to_owned().map_err(git)?,
                committer: decoded.committer().map_err(git)?.to_owned().map_err(git)?,
                encoding: None,
                message: decoded.message.to_owned(),
                extra_headers: Vec::new(),
            };
            Ok(repo.write_object(&commit).map_err(git)?.detach())
        };
        for id in &old {
            let commit = other.find_commit(*id).map_err(git)?;
            let mut files = Files::new();
            files_of(&other, commit.tree_id().map_err(git)?.detach(), "", &mut files)?;
            let mut editor = self.repo.edit_tree(gix::ObjectId::empty_tree(self.repo.object_hash())).map_err(git)?;
            for (path, (kind, blob)) in
                files.iter().filter(|(path, _)| path.starts_with("snapshot/") || *path == "NOTES.md")
            {
                let here = match copied.get(blob) {
                    Some(here) => *here,
                    None => {
                        let data = other.find_blob(*blob).map_err(git)?.data.clone();
                        let here = self.repo.write_blob(&data).map_err(git)?.detach();
                        copied.insert(*blob, here);
                        here
                    }
                };
                editor.upsert(path.as_str(), *kind, here).map_err(git)?;
            }
            let tree = editor.write().map_err(git)?.detach();
            parent = Some(write(&self.repo, &commit, tree, parent)?);
        }
        // this repository's own commits, unchanged but for where they stand
        for id in &own {
            let commit = self.repo.find_commit(*id).map_err(git)?;
            let tree = commit.tree_id().map_err(git)?.detach();
            parent = Some(write(&self.repo, &commit, tree, parent)?);
        }
        if let Some(tip) = parent {
            self.repo
                .reference(
                    BRANCH,
                    tip,
                    gix::refs::transaction::PreviousValue::Any,
                    "moi: history of the previous tool imported",
                )
                .map_err(git)?;
        }
        Ok(old.len())
    }

    /// A commit and what it changed. `rev` is git revision syntax (`HEAD`, `HEAD~3`, an id prefix).
    pub fn show(&self, rev: &str) -> Result<(Entry, Vec<Change>), Error> {
        let id = self
            .repo
            .rev_parse_single(rev)
            .map_err(|_| Error::History(format!("no such revision: {rev} (see `moi log`)")))?
            .detach();
        let commit = self.repo.find_commit(id).map_err(|_| Error::History(format!("{rev} is not a commit")))?;
        let parent = commit.parent_ids().next().and_then(|p| self.repo.find_commit(p.detach()).ok());
        let old = parent.as_ref().and_then(|p| p.tree_id().ok()).map(gix::Id::detach);
        let changes = self.changes_between(old, commit.tree_id().map_err(git)?.detach())?;
        Ok((self.entry(id)?, changes))
    }

    /// `volatile/changelog.txt`: the last commits, for the context file.
    pub fn changelog(&self) -> Result<String, Error> {
        let mut out = String::new();
        for e in self.log(CHANGELOG_COMMITS)? {
            out.push_str(&format!("### {} — {}\n", display_time(&e.time), e.subject));
            if !e.body.is_empty() {
                out.push_str(&e.body);
                out.push('\n');
            }
        }
        Ok(out)
    }

    fn files(&self, tree: gix::ObjectId, prefix: &str, out: &mut Files) -> Result<(), Error> {
        files_of(&self.repo, tree, prefix, out)
    }

    fn changes_between(&self, old: Option<gix::ObjectId>, new: gix::ObjectId) -> Result<Vec<Change>, Error> {
        let (mut before, mut after) = (Files::new(), Files::new());
        if let Some(old) = old {
            self.files(old, "", &mut before)?;
        }
        self.files(new, "", &mut after)?;
        let blob =
            |id: &gix::ObjectId| -> Result<Vec<u8>, Error> { Ok(self.repo.find_blob(*id).map_err(git)?.data.clone()) };
        let mut out = Vec::new();
        for (path, (kind, id)) in &after {
            match before.get(path) {
                None => out.push(Change { status: Status::Added, path: path.clone(), old: None, new: Some(blob(id)?) }),
                Some((old_kind, old_id)) if old_kind != kind || old_id != id => out.push(Change {
                    status: Status::Modified,
                    path: path.clone(),
                    old: Some(blob(old_id)?),
                    new: Some(blob(id)?),
                }),
                Some(_) => {}
            }
        }
        for (path, (_, id)) in &before {
            if !after.contains_key(path) {
                out.push(Change { status: Status::Deleted, path: path.clone(), old: Some(blob(id)?), new: None });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
}

/// Every file of a tree, by path.
fn files_of(repo: &gix::Repository, tree: gix::ObjectId, prefix: &str, out: &mut Files) -> Result<(), Error> {
    let tree = repo.find_tree(tree).map_err(git)?;
    for entry in tree.iter() {
        let entry = entry.map_err(git)?;
        let path = format!("{prefix}{}", entry.filename().to_str_lossy());
        match entry.kind() {
            EntryKind::Tree => files_of(repo, entry.object_id(), &format!("{path}/"), out)?,
            kind => {
                out.insert(path, (kind, entry.object_id()));
            }
        }
    }
    Ok(())
}

/// `2026-11-26T17:00:00Z` → `2026-11-26 17:00` (UTC).
pub fn display_time(rfc3339: &str) -> String {
    rfc3339.get(..16).map_or_else(|| rfc3339.to_string(), |s| s.replace('T', " "))
}

const MANIFEST: &str = "snapshot/files.manifest.tsv";

/// A state path the way people know it: `snapshot/files/etc/hosts` is `/etc/hosts`.
pub fn host_path(path: &str) -> Option<&str> {
    path.strip_prefix("snapshot/files").filter(|p| p.starts_with('/'))
}

/// `<areas>, <up to 4 file paths> +N files`.
fn subject(changes: &[Change]) -> String {
    let mut areas = std::collections::BTreeSet::new();
    let mut files = Vec::new();
    for c in changes.iter().filter(|c| c.path != MANIFEST) {
        if let Some(file) = host_path(&c.path) {
            files.push(file);
        } else if let Some(area) = c.path.strip_prefix("snapshot/") {
            areas.insert(area.split('/').next().unwrap_or(area));
        } else {
            areas.insert(c.path.as_str());
        }
    }
    let mut parts: Vec<&str> = areas.into_iter().collect();
    parts.extend(files.iter().take(4));
    let mut subject = if parts.is_empty() { "misc".to_string() } else { parts.join(", ") };
    if files.len() > 4 {
        subject.push_str(&format!(" +{} files", files.len() - 4));
    }
    subject
}

/// Container and service differences in words, then name-status lines.
fn body(changes: &[Change]) -> String {
    let mut lines = Vec::new();
    for (path, noun) in [("snapshot/docker/containers.json", "container"), ("snapshot/docker/services.json", "service")]
    {
        if let Some(c) = changes.iter().find(|c| c.path == path) {
            lines.extend(describe_list(noun, c.old.as_deref(), c.new.as_deref()));
        }
    }
    let status: Vec<String> = changes
        .iter()
        .filter(|c| c.path != MANIFEST)
        .map(|c| format!("{}\t{}", c.status.letter(), host_path(&c.path).unwrap_or(&c.path)))
        .collect();
    let more = status.len().saturating_sub(MAX_STATUS_LINES);
    lines.extend(status.into_iter().take(MAX_STATUS_LINES));
    if more > 0 {
        lines.push(format!("… and {more} more"));
    }
    lines.join("\n")
}

/// `+ container x (image)`, `- container y (image)`, `~ container z: env, mounts`.
fn describe_list(noun: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> Vec<String> {
    let parse = |bytes: Option<&[u8]>| -> BTreeMap<String, Value> {
        let list: Vec<Value> = bytes.and_then(|b| serde_json::from_slice(b).ok()).unwrap_or_default();
        list.into_iter().filter_map(|item| Some((item["name"].as_str()?.to_string(), item))).collect()
    };
    let (old, new) = (parse(old), parse(new));
    let image = |item: &Value| item["image"].as_str().unwrap_or_default().to_string();
    let mut out = Vec::new();
    for (name, item) in &new {
        match old.get(name) {
            None => out.push(format!("+ {noun} {name} ({})", image(item))),
            Some(before) if before != item => {
                let keys: std::collections::BTreeSet<&String> =
                    before.as_object().into_iter().chain(item.as_object()).flat_map(|o| o.keys()).collect();
                let changed: Vec<&str> =
                    keys.into_iter().filter(|k| before[k.as_str()] != item[k.as_str()]).map(String::as_str).collect();
                out.push(format!("~ {noun} {name}: {}", changed.join(", ")));
            }
            Some(_) => {}
        }
    }
    for (name, item) in &old {
        if !new.contains_key(name) {
            out.push(format!("- {noun} {name} ({})", image(item)));
        }
    }
    out.sort_by(|a, b| a[2..].cmp(&b[2..]));
    out
}

/// Record the current state: commit if something changed and refresh `volatile/changelog.txt`.
pub fn record(host: &dyn Host, state: &StateDir) -> Result<Option<Entry>, Error> {
    let history = History::open(host, state)?;
    let committed = history.commit(host, state)?;
    let changelog = state.volatile().join("changelog.txt");
    let text = history.changelog()?;
    if host.read_to_string(&changelog).ok().as_deref() != Some(text.as_str()) {
        host.create_dir_all(&state.volatile(), 0o700).map_err(|e| Error::host("cannot write changelog", e))?;
        host.write(&changelog, text.as_bytes(), 0o600).map_err(|e| Error::host("cannot write changelog", e))?;
    }
    Ok(committed)
}

/// True if this process may show secrets from the state dir: only root can read it anyway.
pub fn is_root(host: &dyn Host) -> bool {
    host.read_to_string(Path::new("/proc/self/status"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| l.strip_prefix("Uid:").map(|rest| rest.split_whitespace().nth(1) == Some("0")))
        })
        .unwrap_or(false)
}
