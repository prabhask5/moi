//! The owner's scripts: everything in `capture.script_dirs`, plus every file that cron jobs and systemd
//! units refer to — and, two levels deep, what those files refer to. A backup script in an odd place
//! is found because a timer runs it, not because someone remembered to list its directory.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::collect::{DATA_SUFFIXES, LIGHT_PRUNE, expand_glob, is_text, lines};
use super::{Collector, Ctx, Output, Walk, walk_files};
use crate::host::{FileKind, Host};

pub(crate) struct Scripts;

/// Places a referenced path is never one of the owner's scripts.
const SKIP_PREFIX: [&str; 21] = [
    "/usr/bin/",
    "/bin/",
    "/usr/sbin/",
    "/sbin/",
    "/usr/lib/",
    "/lib/",
    "/lib64/",
    "/usr/share/",
    "/dev/",
    "/proc/",
    "/sys/",
    "/tmp/",
    "/run/",
    "/var/run/",
    "/var/log/",
    "/etc/",
    "/var/lib/docker/",
    "/var/cache/",
    "/snap/",
    "/usr/libexec/",
    "/usr/local/lib/python",
];

fn path_char(c: char) -> bool {
    c.is_alphanumeric() || "_.@%+:,=-".contains(c)
}

/// Absolute paths mentioned in a line of shell, cron or unit-file text.
pub(crate) fn paths_in(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i] == '/' && (i == 0 || !(chars[i - 1].is_alphanumeric() || "_.$~-".contains(chars[i - 1])));
        if !starts {
            i += 1;
            continue;
        }
        let mut j = i;
        // segments: "/" followed by at least one path character
        while j < chars.len() && chars[j] == '/' && chars.get(j + 1).copied().is_some_and(path_char) {
            j += 1;
            while j < chars.len() && path_char(chars[j]) {
                j += 1;
            }
        }
        if j > i {
            let path: String = chars[i..j].iter().collect();
            let path = path.trim_end_matches(['.', ',', ':', ';']);
            if path.len() > 1 {
                out.push(path.to_string());
            }
        }
        i = j.max(i + 1);
    }
    out
}

/// Files that schedule or start things.
fn schedulers(host: &dyn Host) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for pattern in [
        "/etc/crontab",
        "/etc/anacrontab",
        "/etc/rc.local",
        "/etc/cron.d/*",
        "/etc/cron.daily/*",
        "/etc/cron.hourly/*",
        "/etc/cron.weekly/*",
        "/etc/cron.monthly/*",
        "/var/spool/cron/crontabs/*",
        "/var/spool/cron/*",
        "/etc/systemd/system/*.service",
        "/etc/systemd/system/*.timer",
        "/etc/systemd/system/*/*.conf",
        "/home/*/.config/systemd/user/*.service",
        "/root/.config/systemd/user/*.service",
    ] {
        out.extend(expand_glob(host, pattern));
    }
    out
}

/// Text files referenced by the schedulers, and by those files, two levels deep.
pub(crate) fn referenced(host: &dyn Host, max_file: u64, state_dir: &str) -> Vec<String> {
    let read_text = |p: &Path| -> Option<String> {
        let md = host.metadata(p).ok()?;
        if md.kind != FileKind::File || md.size > max_file {
            return None;
        }
        let data = host.read(p).ok()?;
        is_text(&data).then(|| String::from_utf8_lossy(&data).into_owned())
    };
    let mut queue: Vec<(String, u8)> = schedulers(host).iter().filter_map(|p| read_text(p)).map(|t| (t, 0)).collect();
    let mut seen = BTreeSet::new();
    let mut out = BTreeSet::new();
    while let Some((text, depth)) = queue.pop() {
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            for path in paths_in(line) {
                if !seen.insert(path.clone()) {
                    continue;
                }
                if SKIP_PREFIX.iter().any(|p| path.starts_with(p)) || path.starts_with(state_dir) {
                    continue;
                }
                let Some(content) = read_text(Path::new(&path)) else { continue };
                out.insert(path);
                if depth < 2 {
                    queue.push((content, depth + 1));
                }
            }
        }
    }
    out.into_iter().collect()
}

/// A pip/setuptools console-script stub: generated on install, not written by anyone.
fn generated_entry_point(text: &str) -> bool {
    let first = text.lines().next().unwrap_or_default();
    first.contains("python")
        && text.lines().take(8).any(|l| {
            l.starts_with("# EASY-INSTALL")
                || l.contains("load_entry_point")
                || (l.starts_with("from ")
                    && l.contains(" import ")
                    && l.split(' ')
                        .nth(1)
                        .is_some_and(|m| m.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')))
        })
}

/// Paths (among `candidates`) that a Debian package installed.
fn dpkg_owned(host: &dyn Host, candidates: &BTreeSet<String>) -> BTreeSet<String> {
    let mut owned = BTreeSet::new();
    let relevant: Vec<&String> =
        candidates.iter().filter(|p| p.starts_with("/opt/") || p.starts_with("/usr/")).collect();
    if relevant.is_empty() {
        return owned;
    }
    let info = Path::new("/var/lib/dpkg/info");
    for name in host.read_dir(info).unwrap_or_default().into_iter().filter(|n| n.ends_with(".list")) {
        let list = host.read_to_string(&info.join(name)).unwrap_or_default();
        for line in list.lines() {
            if relevant.iter().any(|p| p.as_str() == line) {
                owned.insert(line.to_string());
            }
        }
    }
    owned
}

impl Collector for Scripts {
    fn name(&self) -> &'static str {
        "scripts"
    }

    fn collect(&self, cx: &Ctx) -> Result<Output, String> {
        let host = cx.host.as_ref();
        let capture = &cx.config.capture;
        let max_file = capture.max_file.bytes();
        let light = |max_depth: usize| Walk {
            max_depth,
            prune_names: LIGHT_PRUNE.iter().map(ToString::to_string).collect(),
            skip_suffixes: DATA_SUFFIXES.iter().map(ToString::to_string).collect(),
            limit: 5000,
            ..Walk::default()
        };
        let mut out = Output::default();
        let mut candidates = BTreeSet::new();
        for pattern in &capture.script_dirs {
            for dir in expand_glob(host, pattern) {
                let files = walk_files(host, &dir, &light(3));
                candidates.extend(files.iter().map(|p| p.display().to_string()));
                out.grab_all(files);
            }
        }
        let referenced = referenced(host, max_file, &cx.config.state.dir);
        out.stable("scripts/referenced.txt", lines(&referenced));
        candidates.extend(referenced.iter().cloned());
        out.grab_all(referenced.iter().map(PathBuf::from));
        for extra in &capture.extra {
            out.grab_all(walk_files(host, Path::new(extra), &light(8)));
        }

        let owned = dpkg_owned(host, &candidates);
        let mut index = Vec::new();
        for path in &candidates {
            let p = Path::new(path);
            let Ok(md) = host.symlink_metadata(p) else { continue };
            if md.kind != FileKind::File || md.size > max_file || owned.contains(path) {
                continue;
            }
            let Ok(data) = host.read(p) else { continue };
            if data.starts_with(b"#!")
                && !generated_entry_point(&String::from_utf8_lossy(&data[..data.len().min(2048)]))
            {
                index.push(path.clone());
            }
        }
        out.stable("scripts/index.txt", lines(index));
        Ok(out)
    }
}
