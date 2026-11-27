//! Writing a snapshot: generated files, verbatim copies of host files, the manifest, and the cleanup of
//! what is no longer produced. Every file in `snapshot/` and `volatile/` has a row in `moi.db`
//! (`snapshot_files`) naming the collector that owns it; for copied host files the row is also the
//! `(size, mtime, inode) → sha256` cache that lets an unchanged file be skipped without reading it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use super::Output;
use crate::error::Error;
use crate::host::{FileKind, Host, Metadata};
use crate::state::StateDir;
use crate::state::db::Db;

/// Files larger than `capture.max_file` are not copied but still hashed for the manifest, up to this
/// size; beyond it the manifest records `-` (hashing tens of gigabytes is not a snapshot's job).
const HASH_LIMIT: u64 = 64 << 20;

const GENERATED: &str = "gen";
const COPIED: &str = "file";
const LINK: &str = "link";
const SKIPPED: &str = "skipped";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    collector: String,
    kind: String,
    /// Host path of a copied file; empty for generated files.
    source: String,
    size: u64,
    mtime_ns: i64,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    /// Hex digest; `-> target` for symlinks; `-` when not hashed.
    sha256: String,
}

pub(crate) struct Outcome {
    pub changed: Vec<String>,
    pub stable_files: usize,
    pub files_read: usize,
}

pub(crate) struct Store<'a> {
    host: &'a dyn Host,
    root: PathBuf,
    db: &'a Db,
    max_file: u64,
    previous: BTreeMap<String, Row>,
    next: BTreeMap<String, Row>,
    changed: BTreeSet<String>,
    files_read: usize,
}

fn io(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::host(format!("cannot {what} {}", path.display()), e)
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Db(e.to_string())
}

fn mtime_ns(md: &Metadata) -> i64 {
    md.mtime.duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

impl<'a> Store<'a> {
    pub fn open(host: &'a dyn Host, state: &StateDir, db: &'a Db, max_file: u64) -> Result<Self, Error> {
        let mut previous = BTreeMap::new();
        let mut stmt = db
            .conn()
            .prepare("SELECT path, collector, kind, source, size, mtime_ns, inode, mode, uid, gid, sha256 FROM snapshot_files")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Row {
                        collector: r.get(1)?,
                        kind: r.get(2)?,
                        source: r.get(3)?,
                        size: r.get::<_, i64>(4)?.cast_unsigned(),
                        mtime_ns: r.get(5)?,
                        inode: r.get::<_, i64>(6)?.cast_unsigned(),
                        mode: r.get(7)?,
                        uid: r.get(8)?,
                        gid: r.get(9)?,
                        sha256: r.get(10)?,
                    },
                ))
            })
            .map_err(db_err)?;
        for row in rows {
            let (path, row) = row.map_err(db_err)?;
            previous.insert(path, row);
        }
        Ok(Self {
            host,
            root: state.state(),
            db,
            max_file,
            previous,
            next: BTreeMap::new(),
            changed: BTreeSet::new(),
            files_read: 0,
        })
    }

    /// Store a collector's output. A path already claimed by an earlier collector is left to it.
    pub fn apply(&mut self, collector: &str, output: Output) -> Result<(), Error> {
        for (top, files) in [("snapshot", output.stable), ("volatile", output.volatile)] {
            for (rel, content) in files {
                let dest = format!("{top}/{rel}");
                if self.next.contains_key(&dest) {
                    continue;
                }
                self.write_if_changed(&dest, &content)?;
                self.next.insert(
                    dest,
                    Row {
                        collector: collector.to_string(),
                        kind: GENERATED.to_string(),
                        source: String::new(),
                        size: content.len() as u64,
                        mtime_ns: 0,
                        inode: 0,
                        mode: 0,
                        uid: 0,
                        gid: 0,
                        sha256: String::new(),
                    },
                );
            }
        }
        let sources: BTreeSet<PathBuf> = output.files.into_iter().collect();
        for source in sources {
            let dest = format!("snapshot/files{}", source.display());
            if self.next.contains_key(&dest) {
                continue;
            }
            if let Some(row) = self.copy(collector, &source, &dest)? {
                self.next.insert(dest, row);
            }
        }
        Ok(())
    }

    /// Keep everything a collector produced last time (it failed or timed out this time).
    pub fn keep_previous(&mut self, collector: &str) {
        for (path, row) in &self.previous {
            if row.collector == collector && !self.next.contains_key(path) {
                self.next.insert(path.clone(), row.clone());
            }
        }
    }

    /// `files.manifest.tsv`: mode, owner, group, path, size, sha256 of every captured host file, sorted
    /// by path.
    pub fn manifest(&self) -> String {
        let users = super::id_names(self.host, "/etc/passwd");
        let groups = super::id_names(self.host, "/etc/group");
        let mut rows: Vec<&Row> = self.next.values().filter(|r| r.kind != GENERATED).collect();
        rows.sort_by(|a, b| a.source.cmp(&b.source));
        let mut out = String::new();
        for r in rows {
            let name = |map: &BTreeMap<u32, String>, id: u32| map.get(&id).cloned().unwrap_or_else(|| id.to_string());
            out.push_str(&format!(
                "{:o}\t{}\t{}\t{}\t{}\t{}\n",
                r.mode,
                name(&users, r.uid),
                name(&groups, r.gid),
                r.source,
                r.size,
                r.sha256
            ));
        }
        out
    }

    /// Remove what nothing produced this time, drop empty directories, and record the new file table.
    pub fn finish(mut self) -> Result<Outcome, Error> {
        for top in ["snapshot", "volatile"] {
            for path in super::list_tree(self.host, &self.root, top) {
                // the changelog is written after the commit that follows a snapshot (state::history)
                if self.next.contains_key(&path) || path == "volatile/changelog.txt" {
                    continue;
                }
                let abs = self.root.join(&path);
                self.host.remove_file(&abs).map_err(|e| io("remove", &abs, e))?;
                if top == "snapshot" {
                    self.changed.insert(path);
                }
            }
            self.remove_empty_dirs(&self.root.join(top));
        }
        // A row whose file is gone (state dir tampered with, failed collector's file deleted) is dropped.
        let root = self.root.clone();
        let host = self.host;
        self.next.retain(|path, row| row.kind == SKIPPED || host.exists(&root.join(path)));

        let tx = self.db.conn().unchecked_transaction().map_err(db_err)?;
        tx.execute("DELETE FROM snapshot_files", []).map_err(db_err)?;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO snapshot_files (path, collector, kind, source, size, mtime_ns, inode, mode, uid, gid, sha256)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )
                .map_err(db_err)?;
            for (path, r) in &self.next {
                insert
                    .execute((
                        path,
                        &r.collector,
                        &r.kind,
                        &r.source,
                        r.size.cast_signed(),
                        r.mtime_ns,
                        r.inode.cast_signed(),
                        r.mode,
                        r.uid,
                        r.gid,
                        &r.sha256,
                    ))
                    .map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)?;
        let stable_files = self.next.iter().filter(|(p, r)| p.starts_with("snapshot/") && r.kind != SKIPPED).count();
        Ok(Outcome { changed: self.changed.into_iter().collect(), stable_files, files_read: self.files_read })
    }

    fn remove_empty_dirs(&self, dir: &Path) {
        for name in self.host.read_dir(dir).unwrap_or_default() {
            let child = dir.join(name);
            if self.host.symlink_metadata(&child).is_ok_and(|m| m.kind == FileKind::Dir) {
                self.remove_empty_dirs(&child);
                let _ = self.host.remove_dir(&child);
            }
        }
    }

    fn mark(&mut self, dest: &str) {
        if dest.starts_with("snapshot/") {
            self.changed.insert(dest.to_string());
        }
    }

    /// Make sure `dest`'s directory exists and nothing of the wrong type is in the way (a path that was
    /// a file last time and is a directory now, or the reverse).
    fn make_room(&self, dest: &str) -> Result<PathBuf, Error> {
        let abs = self.root.join(dest);
        let mut prefix = self.root.clone();
        let parts: Vec<&str> = dest.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            prefix.push(part);
            if self.host.symlink_metadata(&prefix).is_ok_and(|m| m.kind != FileKind::Dir) {
                self.host.remove_file(&prefix).map_err(|e| io("replace", &prefix, e))?;
            }
        }
        if let Some(parent) = abs.parent() {
            self.host.create_dir_all(parent, 0o700).map_err(|e| io("create", parent, e))?;
        }
        if self.host.symlink_metadata(&abs).is_ok_and(|m| m.kind == FileKind::Dir) {
            self.remove_tree(&abs)?;
        }
        Ok(abs)
    }

    fn remove_tree(&self, dir: &Path) -> Result<(), Error> {
        for name in self.host.read_dir(dir).unwrap_or_default() {
            let child = dir.join(name);
            if self.host.symlink_metadata(&child).is_ok_and(|m| m.kind == FileKind::Dir) {
                self.remove_tree(&child)?;
            } else {
                self.host.remove_file(&child).map_err(|e| io("remove", &child, e))?;
            }
        }
        self.host.remove_dir(dir).map_err(|e| io("remove", dir, e))
    }

    fn write_if_changed(&mut self, dest: &str, content: &[u8]) -> Result<(), Error> {
        let abs = self.root.join(dest);
        let same = self.host.symlink_metadata(&abs).is_ok_and(|m| m.kind == FileKind::File)
            && self.host.read(&abs).is_ok_and(|old| old == content);
        if same {
            return Ok(());
        }
        let abs = self.make_room(dest)?;
        self.host.write(&abs, content, 0o600).map_err(|e| io("write", &abs, e))?;
        self.mark(dest);
        Ok(())
    }

    /// Copy one host file or symlink. `None` if it vanished or is neither.
    fn copy(&mut self, collector: &str, source: &Path, dest: &str) -> Result<Option<Row>, Error> {
        let Ok(md) = self.host.symlink_metadata(source) else { return Ok(None) };
        let mut row = Row {
            collector: collector.to_string(),
            kind: COPIED.to_string(),
            source: source.to_string_lossy().into_owned(),
            size: md.size,
            mtime_ns: mtime_ns(&md),
            inode: md.inode,
            mode: md.mode,
            uid: md.uid,
            gid: md.gid,
            sha256: String::new(),
        };
        let abs = self.root.join(dest);
        let on_disk = self.host.symlink_metadata(&abs).map(|m| m.kind).ok();
        match md.kind {
            FileKind::Symlink => {
                let Ok(target) = self.host.read_link(source) else { return Ok(None) };
                row.kind = LINK.to_string();
                row.sha256 = format!("-> {}", target.display());
                let same = on_disk == Some(FileKind::Symlink) && self.host.read_link(&abs).is_ok_and(|t| t == target);
                if !same {
                    let abs = self.make_room(dest)?;
                    self.host.symlink(&target, &abs).map_err(|e| io("write", &abs, e))?;
                    self.mark(dest);
                }
            }
            FileKind::File => {
                let too_big = md.size > self.max_file;
                row.kind = if too_big { SKIPPED } else { COPIED }.to_string();
                // Unchanged since last time: neither read nor rewritten.
                if let Some(old) = self.previous.get(dest) {
                    let unchanged =
                        Row { collector: old.collector.clone(), sha256: old.sha256.clone(), ..row.clone() } == *old;
                    if unchanged && (too_big || on_disk == Some(FileKind::File)) {
                        row.sha256.clone_from(&old.sha256);
                        return Ok(Some(row));
                    }
                }
                if too_big {
                    row.sha256 = if md.size <= HASH_LIMIT {
                        self.files_read += 1;
                        self.host.read(source).map_or_else(|_| "-".to_string(), |data| sha256_hex(&data))
                    } else {
                        "-".to_string()
                    };
                    if on_disk.is_some() {
                        self.host.remove_file(&abs).map_err(|e| io("remove", &abs, e))?;
                        self.mark(dest);
                    }
                } else {
                    let Ok(data) = self.host.read(source) else { return Ok(None) };
                    self.files_read += 1;
                    row.size = data.len() as u64;
                    row.sha256 = sha256_hex(&data);
                    self.write_if_changed(dest, &data)?;
                }
            }
            FileKind::Dir | FileKind::Other => return Ok(None),
        }
        Ok(Some(row))
    }
}
