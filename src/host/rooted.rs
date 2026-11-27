//! Path resolution inside one or more root directories ("layers"), plus the file operations both hosts
//! share. A layered root is how `FixtureHost` puts a writable overlay over a read-only fixture.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use super::{FileKind, Metadata};

/// Symlink hops before giving up (Linux uses 40).
const MAX_HOPS: usize = 40;

/// A host path resolved against the layers.
#[derive(Debug)]
pub(crate) struct Resolved {
    /// Normalized path inside the host (no `.`/`..`, symlinks followed as requested).
    pub logical: PathBuf,
    /// Where it is on the real filesystem: in the first layer that has it, else in layer 0.
    pub real: PathBuf,
    /// Index of the layer that has it, `None` if it exists in no layer.
    pub layer: Option<usize>,
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

fn in_layer(layer: &Path, logical: &[OsString]) -> PathBuf {
    let mut p = layer.to_path_buf();
    p.extend(logical);
    p
}

fn find(layers: &[&Path], logical: &[OsString]) -> Option<(usize, PathBuf)> {
    layers.iter().enumerate().find_map(|(i, layer)| {
        let real = in_layer(layer, logical);
        fs::symlink_metadata(&real).is_ok().then_some((i, real))
    })
}

/// Resolve an absolute host path. Intermediate symlinks are always followed; the last component only
/// when `follow_last`. Absolute symlink targets restart at the host root, `..` never climbs above it.
pub(crate) fn resolve(layers: &[&Path], path: &Path, follow_last: bool) -> io::Result<Resolved> {
    if !path.is_absolute() {
        return Err(invalid(format!("path must be absolute: {}", path.display())));
    }
    let mut pending: VecDeque<OsString> = VecDeque::new();
    push_components(&mut pending, path, false);
    let mut cur: Vec<OsString> = Vec::new();
    let mut hops = 0;
    while let Some(comp) = pending.pop_front() {
        if comp == ".." {
            cur.pop();
            continue;
        }
        cur.push(comp);
        let is_last = pending.is_empty();
        if !follow_last && is_last {
            break;
        }
        let Some((_, real)) = find(layers, &cur) else { continue };
        if fs::symlink_metadata(&real)?.file_type().is_symlink() {
            hops += 1;
            if hops > MAX_HOPS {
                return Err(invalid(format!("too many levels of symbolic links: {}", path.display())));
            }
            let target = fs::read_link(&real)?;
            cur.pop();
            if target.is_absolute() {
                cur.clear();
            }
            push_components(&mut pending, &target, true);
        }
    }
    let found = find(layers, &cur);
    let logical: PathBuf = Path::new("/").join(cur.iter().collect::<PathBuf>());
    let first = layers.first().ok_or_else(|| invalid("host has no root".to_string()))?;
    Ok(match found {
        Some((layer, real)) => Resolved { logical, real, layer: Some(layer) },
        None => Resolved { logical, real: in_layer(first, &cur), layer: None },
    })
}

/// Queue the components of `path`; `front` inserts them before what is already pending (symlink targets).
fn push_components(pending: &mut VecDeque<OsString>, path: &Path, front: bool) {
    let comps: Vec<OsString> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_os_string()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect();
    if front {
        for c in comps.into_iter().rev() {
            pending.push_front(c);
        }
    } else {
        pending.extend(comps);
    }
}

pub(crate) fn to_metadata(md: &fs::Metadata) -> Metadata {
    let ft = md.file_type();
    let kind = if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    };
    Metadata {
        kind,
        size: md.len(),
        mode: md.mode() & 0o7777,
        uid: md.uid(),
        gid: md.gid(),
        mtime: md.modified().unwrap_or(std::time::UNIX_EPOCH),
        inode: md.ino(),
    }
}

pub(crate) fn dir_names(real: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(real)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

/// Write via a temp file in the same directory and rename over the target.
pub(crate) fn write_atomic(real: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let dir = real.parent().ok_or_else(|| invalid(format!("no parent directory: {}", real.display())))?;
    let name = real.file_name().ok_or_else(|| invalid(format!("not a file path: {}", real.display())))?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".moi-tmp.{}", std::process::id()));
    let tmp = dir.join(tmp_name);
    let result = (|| {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
        // `mode` above is filtered by the umask; set it exactly.
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, real)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `mkdir -p` where every directory this call creates gets exactly `mode`.
pub(crate) fn create_dir_all(real: &Path, mode: u32) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut cur = real;
    while fs::symlink_metadata(cur).is_err() {
        missing.push(cur);
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    for dir in missing.into_iter().rev() {
        match fs::create_dir(dir) {
            Ok(()) => fs::set_permissions(dir, fs::Permissions::from_mode(mode))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    if fs::metadata(real)?.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("not a directory: {}", real.display())))
    }
}

pub(crate) fn set_mode(real: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(real, fs::Permissions::from_mode(mode))
}
