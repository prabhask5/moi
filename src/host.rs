//! All system access: files under a root prefix, `/proc`, `/sys`, commands, environment, clock, journal.
//!
//! [`RealHost`] is production, [`FixtureHost`] is for tests and for running the binary against a fixture
//! directory (`MOI_ROOT`). Nothing else in the crate touches the OS directly, so every feature can be
//! tested against `tests/fixtures/hosts/<name>/` (CLAUDE.md §3).
//!
//! Paths given to a host are always absolute *inside the host*: `/etc/hosts` is the host's hosts file
//! wherever its root really is. Symlinks (also absolute ones) and `..` are resolved inside the root and
//! can never escape it.

mod fake_docker;
mod fixture;
mod real;
mod rooted;
mod time;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use fixture::FixtureHost;
pub use real::RealHost;
pub use time::{format_rfc3339, parse_rfc3339, unix_seconds};

/// What a path is (after or without following symlinks, see [`Host::metadata`] / [`Host::symlink_metadata`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// The parts of `stat` moi uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub kind: FileKind,
    pub size: u64,
    /// Permission bits only (`0o7777` mask).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: SystemTime,
    pub inode: u64,
    /// Device the file is on: differs between filesystems (used to stay on one).
    pub dev: u64,
}

/// An external command to run. Built with the chained setters; never goes through a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Self { program: program.into(), args: Vec::new(), env: Vec::new(), stdin: None, timeout: None }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Extra environment variable for the child (values are never part of [`Cmd::line`]).
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    #[must_use]
    pub fn stdin(mut self, data: Vec<u8>) -> Self {
        self.stdin = Some(data);
        self
    }

    /// Kill the command if it runs longer than this.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Program and arguments joined by single spaces: the key fixtures use for canned outputs, and what
    /// error messages show.
    pub fn line(&self) -> String {
        let mut line = self.program.clone();
        for a in &self.args {
            line.push(' ');
            line.push_str(a);
        }
        line
    }
}

/// Result of a finished (or killed) command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdOutput {
    /// Exit code; `128 + signal` when killed by a signal.
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// True when the command was killed because it exceeded [`Cmd::timeout`].
    pub timed_out: bool,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.status == 0 && !self.timed_out
    }

    pub fn stdout_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    pub fn stderr_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

/// A connected stream socket (the Docker engine's unix socket).
pub trait Conn: io::Read + io::Write + Send + fmt::Debug {
    /// Fail reads and writes that block longer than this; `None` waits forever.
    fn set_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
}

/// The operating system as moi sees it.
pub trait Host: fmt::Debug + Send + Sync {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        String::from_utf8(self.read(path)?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{} is not valid UTF-8", path.display())))
    }

    /// `stat`: follows symlinks.
    fn metadata(&self, path: &Path) -> io::Result<Metadata>;

    /// `lstat`: describes a symlink itself.
    fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata>;

    /// Target of a symlink, exactly as stored (may be absolute inside the host, or relative).
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;

    /// Entry names of a directory, sorted.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>>;

    /// True if the path exists (a dangling symlink exists).
    fn exists(&self, path: &Path) -> bool {
        self.symlink_metadata(path).is_ok()
    }

    /// Write a file atomically (temp file in the same directory, then rename) with these permission bits.
    fn write(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()>;

    /// Create a directory and its missing parents; directories it creates get `mode`.
    fn create_dir_all(&self, path: &Path, mode: u32) -> io::Result<()>;

    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Remove an empty directory.
    fn remove_dir(&self, path: &Path) -> io::Result<()>;

    /// Create a symlink at `path` pointing to `target` (stored as given), replacing a file or symlink
    /// already there.
    fn symlink(&self, target: &Path, path: &Path) -> io::Result<()>;

    /// `chmod`.
    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()>;

    /// Where a host path really is on this machine's filesystem, for libraries that need a path to open
    /// (SQLite, gitoxide). The file itself may not exist yet; its directory must.
    fn real_path(&self, path: &Path) -> io::Result<PathBuf>;

    /// Run an external command and wait for it. A non-zero exit is not an error; failing to start is.
    fn run(&self, cmd: &Cmd) -> io::Result<CmdOutput>;

    /// Connect to a unix stream socket (Docker's `/var/run/docker.sock`).
    fn connect_unix(&self, path: &Path) -> io::Result<Box<dyn Conn>>;

    fn now(&self) -> SystemTime;

    fn env(&self, name: &str) -> Option<String>;

    /// Send one entry to the systemd journal (native protocol fields, see `log`).
    fn journal(&self, fields: &[(String, String)]) -> io::Result<()>;
}

/// The machine's short hostname (up to the first dot); `localhost` if it cannot be read.
pub fn short_hostname(host: &dyn Host) -> String {
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(text) = host.read_to_string(Path::new(path)) {
            let name = text.trim().split('.').next().unwrap_or_default();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    "localhost".to_string()
}

/// The system timezone name (`America/New_York`) from `/etc/timezone` or the `/etc/localtime` symlink.
pub fn timezone(host: &dyn Host) -> Option<String> {
    if let Ok(text) = host.read_to_string(Path::new("/etc/timezone")) {
        let tz = text.trim();
        if !tz.is_empty() {
            return Some(tz.to_string());
        }
    }
    let target = host.read_link(Path::new("/etc/localtime")).ok()?;
    let target = target.to_str()?;
    let (_, tz) = target.split_once("zoneinfo/")?;
    (!tz.is_empty()).then(|| tz.to_string())
}
