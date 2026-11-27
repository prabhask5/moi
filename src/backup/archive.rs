//! The local archive: a gzip-compressed tar of the path set, written file by file through `Host`.
//! SQLite databases are not copied as they lie — a live file can be mid-transaction — but through
//! SQLite's online backup into a staging file, which goes into the archive under the original name.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use rusqlite::{Connection, OpenFlags};

use super::plan::{Excluder, walk};
use crate::host::{FileKind, Host, Metadata, unix_seconds};

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// What archiving did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Archived {
    pub files: u64,
    /// SQLite databases taken through the online backup.
    pub databases: Vec<String>,
    /// Things tar would warn about and go on: files that vanished or changed while being read.
    pub warnings: Vec<String>,
}

/// Reads exactly `size` bytes: a file that shrank is padded, one that grew is cut (the header is
/// already written).
struct Exactly<R> {
    inner: R,
    left: u64,
    short: bool,
}

impl<R: Read> Read for Exactly<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = usize::try_from(self.left).unwrap_or(usize::MAX).min(buf.len());
        let got = if self.short { 0 } else { self.inner.read(&mut buf[..want])? };
        let got = if got == 0 {
            self.short = true;
            buf[..want].fill(0);
            want
        } else {
            got
        };
        self.left -= got as u64;
        Ok(got)
    }
}

fn is_sqlite(host: &dyn Host, path: &Path, meta: &Metadata) -> bool {
    if meta.size < 100 {
        return false;
    }
    let mut magic = [0u8; 16];
    host.open(path).is_ok_and(|mut f| f.read_exact(&mut magic).is_ok() && &magic == SQLITE_MAGIC)
}

/// Copy a live SQLite database into `staged` with the online backup API: a consistent copy without
/// stopping the application. The source is opened read-only, as a second choice immutable (when even
/// its lock files may not be created).
pub fn stage_sqlite(host: &dyn Host, source: &Path, staged: &Path) -> Result<(), String> {
    let (source, target) =
        (host.real_path(source).map_err(|e| e.to_string())?, host.real_path(staged).map_err(|e| e.to_string())?);
    let mut last = String::new();
    for uri in [format!("file:{}?mode=ro", source.display()), format!("file:{}?mode=ro&immutable=1", source.display())]
    {
        let _ = std::fs::remove_file(&target);
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let copied = Connection::open_with_flags(&uri, flags).and_then(|from| {
            let mut to = Connection::open(&target)?;
            let backup = rusqlite::backup::Backup::new(&from, &mut to)?;
            backup.run_to_completion(2048, std::time::Duration::from_millis(5), None)
        });
        match copied {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

fn header(meta: &Metadata, kind: tar::EntryType, size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(meta.mode);
    header.set_uid(u64::from(meta.uid));
    header.set_gid(u64::from(meta.gid));
    header.set_mtime(u64::try_from(unix_seconds(meta.mtime)).unwrap_or(0));
    header.set_size(size);
    header
}

/// Write the archive of `roots` to `target`. `staging` is an empty directory for SQLite copies.
pub fn write(
    host: &dyn Host,
    roots: &[String],
    excluder: &Excluder,
    target: &Path,
    staging: &Path,
) -> io::Result<Archived> {
    let out = host.create(target, 0o600)?;
    let mut tar = tar::Builder::new(GzEncoder::new(out, Compression::default()));
    let mut done = Archived::default();
    let mut entries: Vec<(PathBuf, Metadata)> = Vec::new();
    for root in roots {
        walk(host, Path::new(root), excluder, &mut |path, meta| entries.push((path.to_path_buf(), meta.clone())));
    }
    // a database's write-ahead files belong to the live file, not to its consistent copy
    let sqlite: Vec<PathBuf> = entries
        .iter()
        .filter(|(path, meta)| meta.kind == FileKind::File && is_sqlite(host, path, meta))
        .map(|(path, _)| path.clone())
        .collect();
    let sidecar = |path: &Path| {
        let name = path.to_string_lossy();
        ["-wal", "-shm", "-journal"]
            .iter()
            .any(|suffix| name.strip_suffix(suffix).is_some_and(|base| sqlite.iter().any(|db| db == Path::new(base))))
    };
    let mut staged_count = 0;
    for (path, meta) in &entries {
        let name = path.strip_prefix("/").unwrap_or(path);
        if name.as_os_str().is_empty() || sidecar(path) {
            continue;
        }
        match meta.kind {
            FileKind::Dir => tar.append_data(&mut header(meta, tar::EntryType::Directory, 0), name, io::empty())?,
            FileKind::Symlink => match host.read_link(path) {
                Ok(link) => tar.append_link(&mut header(meta, tar::EntryType::Symlink, 0), name, link)?,
                Err(_) => done.warnings.push(format!("{}: file removed before we read it", path.display())),
            },
            FileKind::File => {
                // a database: its consistent copy, under its own name
                let mut source = path.clone();
                let mut size = meta.size;
                if sqlite.contains(path) {
                    let staged = staging.join(format!("{staged_count}.sqlite"));
                    staged_count += 1;
                    match stage_sqlite(host, path, &staged)
                        .and_then(|()| host.metadata(&staged).map_err(|e| e.to_string()))
                    {
                        Ok(copy) => {
                            (source, size) = (staged, copy.size);
                            done.databases.push(path.to_string_lossy().into_owned());
                        }
                        Err(e) => done.warnings.push(format!(
                            "{}: SQLite online backup failed ({e}); the file is copied as it lies",
                            path.display()
                        )),
                    }
                }
                match host.open(&source) {
                    Ok(file) => {
                        let mut reader = Exactly { inner: file, left: size, short: false };
                        tar.append_data(&mut header(meta, tar::EntryType::Regular, size), name, &mut reader)?;
                        if reader.short {
                            done.warnings.push(format!("{}: file changed as we read it", path.display()));
                        }
                        done.files += 1;
                    }
                    Err(_) => done.warnings.push(format!("{}: file removed before we read it", path.display())),
                }
                if source != *path {
                    let _ = host.remove_file(&source);
                }
            }
            // sockets, devices, pipes: nothing to keep
            FileKind::Other => {}
        }
    }
    tar.into_inner()?.finish()?.flush()?;
    Ok(done)
}
