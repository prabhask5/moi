//! A fake machine read from a fixture directory (`tests/fixtures/README.md`):
//!
//! ```text
//! <dir>/root/      the filesystem (read-only)
//! <dir>/commands/  canned command outputs: <key>.out, optional <key>.err and <key>.exit
//! <dir>/clock      fixed "now" (RFC 3339)
//! <dir>/modes      "mode uid gid path" lines for what git cannot store
//! ```
//!
//! Writes go to an optional overlay directory that shadows `root/`; without one the host is read-only.
//! Commands never run, the clock only moves when told, the environment is empty unless set, and journal
//! entries are kept in memory.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::fake_docker::{self, DockerState, FakeConn};
use super::rooted::{self, Resolved};
use super::{Cmd, CmdOutput, Conn, Host, Metadata};

/// 2026-01-01T00:00:00Z, used when the fixture has no `clock` file.
const DEFAULT_CLOCK: u64 = 1_767_225_600;

#[derive(Debug)]
pub struct FixtureHost {
    dir: PathBuf,
    base: PathBuf,
    overlay: Option<PathBuf>,
    modes: BTreeMap<PathBuf, (u32, u32, u32)>,
    state: Mutex<State>,
    docker: Arc<Mutex<DockerState>>,
}

#[derive(Debug)]
struct State {
    now: SystemTime,
    env: BTreeMap<String, String>,
    commands: BTreeMap<String, CmdOutput>,
    ran: Vec<String>,
    journal: Vec<Vec<(String, String)>>,
}

fn bad_fixture(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

impl FixtureHost {
    /// Open a fixture directory. It must contain `root/`.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let base = dir.join("root");
        if !base.is_dir() {
            return Err(bad_fixture(format!("{} is not a fixture host: it has no root/ directory", dir.display())));
        }
        let now = match fs::read_to_string(dir.join("clock")) {
            Ok(text) => super::parse_rfc3339(&text).ok_or_else(|| {
                bad_fixture(format!("{}/clock is not an RFC 3339 time: {}", dir.display(), text.trim()))
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => UNIX_EPOCH + Duration::from_secs(DEFAULT_CLOCK),
            Err(e) => return Err(e),
        };
        let mut modes = BTreeMap::new();
        match fs::read_to_string(dir.join("modes")) {
            Ok(text) => {
                for (n, line) in text.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let mut parts = line.splitn(4, char::is_whitespace);
                    let parsed = (|| {
                        let mode = u32::from_str_radix(parts.next()?, 8).ok()?;
                        let uid = parts.next()?.parse().ok()?;
                        let gid = parts.next()?.parse().ok()?;
                        let path = parts.next()?.trim();
                        path.starts_with('/').then(|| (PathBuf::from(path), (mode, uid, gid)))
                    })();
                    let (path, entry) = parsed.ok_or_else(|| {
                        bad_fixture(format!("{}/modes:{}: expected `mode uid gid /path`", dir.display(), n + 1))
                    })?;
                    modes.insert(path, entry);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            base,
            overlay: None,
            modes,
            state: Mutex::new(State {
                now,
                env: BTreeMap::new(),
                commands: BTreeMap::new(),
                ran: Vec::new(),
                journal: Vec::new(),
            }),
            docker: Arc::default(),
        })
    }

    /// Send writes to `overlay` (created if missing); files there shadow the fixture's.
    pub fn with_overlay(mut self, overlay: &Path) -> io::Result<Self> {
        fs::create_dir_all(overlay)?;
        self.overlay = Some(overlay.to_path_buf());
        Ok(self)
    }

    /// Name of the files holding the canned output of a command line: 64-bit FNV-1a, 16 hex digits.
    pub fn command_key(line: &str) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in line.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    /// Can a command's output in memory (wins over the fixture's `commands/` directory).
    pub fn add_command(&self, line: &str, status: i32, stdout: &str) {
        let out = CmdOutput { status, stdout: stdout.as_bytes().to_vec(), ..CmdOutput::default() };
        self.state().commands.insert(line.to_string(), out);
    }

    /// Every command line that was run, in order.
    pub fn commands_run(&self) -> Vec<String> {
        self.state().ran.clone()
    }

    pub fn set_env(&self, name: &str, value: &str) {
        self.state().env.insert(name.to_string(), value.to_string());
    }

    pub fn set_now(&self, now: SystemTime) {
        self.state().now = now;
    }

    /// Move the clock forward.
    pub fn advance(&self, by: Duration) {
        let mut state = self.state();
        state.now += by;
    }

    /// Journal entries sent so far, each a list of fields.
    pub fn journal_entries(&self) -> Vec<Vec<(String, String)>> {
        self.state().journal.clone()
    }

    /// Every request the fake Docker engine received, as `METHOD /path?query`.
    pub fn docker_requests(&self) -> Vec<String> {
        self.docker.lock().unwrap_or_else(PoisonError::into_inner).requests.clone()
    }

    /// Answer `GET <path>` (no query) with this JSON instead of the fixture's file: how a test changes
    /// the engine's state between two snapshots.
    pub fn set_docker(&self, path: &str, json: &str) {
        self.docker.lock().unwrap_or_else(PoisonError::into_inner).overrides.insert(path.to_string(), json.to_string());
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn layers(&self) -> Vec<&Path> {
        self.overlay.iter().map(PathBuf::as_path).chain(std::iter::once(self.base.as_path())).collect()
    }

    fn resolve(&self, path: &Path, follow_last: bool) -> io::Result<Resolved> {
        rooted::resolve(&self.layers(), path, follow_last)
    }

    fn stat(&self, path: &Path, follow_last: bool) -> io::Result<Metadata> {
        let r = self.resolve(path, follow_last)?;
        let mut md = rooted::to_metadata(&fs::symlink_metadata(&r.real)?);
        // Checked-out fixtures belong to whoever ran git; a server's files belong to root unless stated.
        (md.uid, md.gid) = (0, 0);
        if let Some(&(mode, uid, gid)) = self.modes.get(&r.logical) {
            let in_overlay = self.overlay.is_some() && r.layer == Some(0);
            if !in_overlay {
                (md.mode, md.uid, md.gid) = (mode, uid, gid);
            }
        }
        Ok(md)
    }

    fn read_only(&self, path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("fixture host {} is read-only (no overlay): cannot change {}", self.dir.display(), path.display()),
        )
    }

    /// Where a write to `path` lands in the overlay. Its parent directory must exist in some layer; it is
    /// mirrored into the overlay if it only exists in the fixture.
    fn writable(&self, path: &Path) -> io::Result<PathBuf> {
        let overlay = self.overlay.as_deref().ok_or_else(|| self.read_only(path))?;
        let r = self.resolve(path, true)?;
        let relative = r.logical.strip_prefix("/").unwrap_or(&r.logical);
        let target = overlay.join(relative);
        if let Some(parent) = r.logical.parent() {
            if !self.stat(parent, true).is_ok_and(|md| md.kind == super::FileKind::Dir) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such directory: {}", parent.display()),
                ));
            }
            if let Some(dir) = target.parent() {
                fs::create_dir_all(dir)?;
            }
        }
        Ok(target)
    }

    /// In the overlay and nowhere else: the only files that may be removed or re-moded.
    fn overlay_only(&self, path: &Path) -> io::Result<PathBuf> {
        if self.overlay.is_none() {
            return Err(self.read_only(path));
        }
        let r = self.resolve(path, false)?;
        match r.layer {
            Some(0) => Ok(r.real),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} belongs to the read-only fixture; write it first to shadow it", path.display()),
            )),
            None => Err(io::Error::new(io::ErrorKind::NotFound, format!("no such file: {}", path.display()))),
        }
    }
}

impl Host for FixtureHost {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(self.resolve(path, true)?.real)
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.stat(path, true)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.stat(path, false)
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        fs::read_link(self.resolve(path, false)?.real)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        let logical = self.resolve(path, true)?.logical;
        let relative = logical.strip_prefix("/").unwrap_or(&logical);
        let mut names = BTreeSet::new();
        let mut found = false;
        for layer in self.layers() {
            let real = layer.join(relative);
            if real.is_dir() {
                found = true;
                names.extend(rooted::dir_names(&real)?);
            }
        }
        if found {
            Ok(names.into_iter().collect())
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, format!("no such directory: {}", path.display())))
        }
    }

    fn write(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        rooted::write_atomic(&self.writable(path)?, data, mode)
    }

    fn create_dir_all(&self, path: &Path, mode: u32) -> io::Result<()> {
        let overlay = self.overlay.as_deref().ok_or_else(|| self.read_only(path))?;
        let logical = self.resolve(path, true)?.logical;
        let relative = logical.strip_prefix("/").unwrap_or(&logical);
        // Parents that only exist in the fixture are mirrored with ordinary permissions; `mode` is for
        // the directories this call really adds to the host.
        let mut existing = relative;
        while !existing.as_os_str().is_empty() && !self.base.join(existing).is_dir() {
            existing = existing.parent().unwrap_or(Path::new(""));
        }
        fs::create_dir_all(overlay.join(existing))?;
        rooted::create_dir_all(&overlay.join(relative), mode)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(self.overlay_only(path)?)
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        let r = self.resolve(path, true)?;
        if self.overlay.is_none() {
            return Err(self.read_only(path));
        }
        match r.layer {
            Some(0) => rooted::set_mode(&r.real, mode),
            Some(_) if r.real.is_dir() => {
                // A fixture directory: shadow it in the overlay and set the mode there.
                let relative = r.logical.strip_prefix("/").unwrap_or(&r.logical);
                let shadow = self.overlay.as_deref().unwrap_or(&self.base).join(relative);
                fs::create_dir_all(&shadow)?;
                rooted::set_mode(&shadow, mode)
            }
            _ => self.overlay_only(path).map(|_| ()),
        }
    }

    fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
        match self.overlay {
            Some(_) => self.writable(path),
            None => Ok(self.resolve(path, true)?.real),
        }
    }

    fn run(&self, cmd: &Cmd) -> io::Result<CmdOutput> {
        let line = cmd.line();
        let mut state = self.state();
        state.ran.push(line.clone());
        if let Some(out) = state.commands.get(&line) {
            return Ok(out.clone());
        }
        drop(state);
        let key = Self::command_key(&line);
        let file = |ext: &str| self.dir.join("commands").join(format!("{key}.{ext}"));
        let stdout = fs::read(file("out")).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("no canned output for `{line}` in this fixture: add {}", file("out").display()),
            )
        })?;
        let status = match fs::read_to_string(file("exit")) {
            Ok(text) => text
                .trim()
                .parse()
                .map_err(|_| bad_fixture(format!("{} is not an exit code", file("exit").display())))?,
            Err(_) => 0,
        };
        Ok(CmdOutput { status, stdout, stderr: fs::read(file("err")).unwrap_or_default(), timed_out: false })
    }

    fn connect_unix(&self, path: &Path) -> io::Result<Box<dyn Conn>> {
        match fake_docker::engine_dir(&self.dir) {
            Some(dir) if path == Path::new(crate::docker::DEFAULT_SOCKET) => {
                Ok(Box::new(FakeConn::new(dir, Arc::clone(&self.docker))))
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, format!("no such socket: {}", path.display()))),
        }
    }

    fn now(&self) -> SystemTime {
        self.state().now
    }

    fn env(&self, name: &str) -> Option<String> {
        self.state().env.get(name).cloned()
    }

    fn journal(&self, fields: &[(String, String)]) -> io::Result<()> {
        self.state().journal.push(fields.to_vec());
        Ok(())
    }
}
