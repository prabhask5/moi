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

/// The fixture's whole network: gets `(host:port, raw HTTP request)` and returns the raw response.
pub type HttpHandler = Arc<dyn Fn(&str, &[u8]) -> Vec<u8> + Send + Sync>;

pub struct FixtureHost {
    dir: PathBuf,
    base: PathBuf,
    overlay: Option<PathBuf>,
    modes: BTreeMap<PathBuf, (u32, u32, u32)>,
    mtimes: BTreeMap<PathBuf, SystemTime>,
    /// Modification time of every fixture file not listed in `mtimes`: 30 days before the clock.
    default_mtime: SystemTime,
    state: Mutex<State>,
    docker: Arc<Mutex<DockerState>>,
    http: Mutex<Option<HttpHandler>>,
}

#[derive(Debug)]
struct State {
    now: SystemTime,
    env: BTreeMap<String, String>,
    commands: BTreeMap<String, CmdOutput>,
    ran: Vec<String>,
    journal: Vec<Vec<(String, String)>>,
    /// Owners given to overlay files with `set_owner` (the test process cannot really chown).
    owners: BTreeMap<PathBuf, (u32, u32)>,
    /// Fixture directories whose mode a test changed (their overlay shadow carries the mode).
    chmodded: BTreeSet<PathBuf>,
    /// How many random bytes were handed out (the fixture's "randomness" is a fixed sequence).
    random_counter: u64,
}

impl std::fmt::Debug for FixtureHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FixtureHost({})", self.dir.display())
    }
}

/// A TCP connection answered by the test's [`HttpHandler`].
struct HandlerConn {
    authority: String,
    handler: HttpHandler,
    request: Vec<u8>,
    response: Option<io::Cursor<Vec<u8>>>,
}

impl std::fmt::Debug for HandlerConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HandlerConn({})", self.authority)
    }
}

impl io::Write for HandlerConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.request.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for HandlerConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (authority, handler, request) = (&self.authority, &self.handler, &self.request);
        self.response.get_or_insert_with(|| io::Cursor::new(handler(authority, request))).read(buf)
    }
}

impl Conn for HandlerConn {
    fn set_timeout(&mut self, _timeout: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
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
        let mut mtimes = BTreeMap::new();
        if let Ok(text) = fs::read_to_string(dir.join("mtimes")) {
            for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty() && !l.starts_with('#')) {
                let parsed = line
                    .split_once(' ')
                    .and_then(|(time, path)| Some((PathBuf::from(path.trim()), super::parse_rfc3339(time)?)));
                let (path, time) = parsed.ok_or_else(|| {
                    bad_fixture(format!("{}/mtimes:{}: expected `<RFC 3339 time> /path`", dir.display(), n + 1))
                })?;
                mtimes.insert(path, time);
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            base,
            overlay: None,
            modes,
            mtimes,
            default_mtime: now - Duration::from_secs(30 * 86_400),
            state: Mutex::new(State {
                now,
                env: BTreeMap::new(),
                commands: BTreeMap::new(),
                ran: Vec::new(),
                journal: Vec::new(),
                owners: BTreeMap::new(),
                chmodded: BTreeSet::new(),
                random_counter: 0,
            }),
            http: Mutex::new(None),
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

    /// Give the fixture a network: every TCP connection is answered by `handler`, which receives the
    /// `host:port` connected to and the raw HTTP request. Without one, connecting is refused.
    pub fn set_http(&self, handler: HttpHandler) {
        *self.http.lock().unwrap_or_else(PoisonError::into_inner) = Some(handler);
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
        let relative = r.logical.strip_prefix("/").unwrap_or(&r.logical);
        // A fixture directory that merely got mirrored into the overlay (something was written below it)
        // is still the fixture's directory, unless a test changed its mode there.
        let mirrored = md.kind == super::FileKind::Dir
            && self.base.join(relative).is_dir()
            && !self.state().chmodded.contains(&r.logical);
        let in_overlay = self.overlay.is_some() && r.layer == Some(0) && !mirrored;
        if !in_overlay {
            // …and their permission bits depend on the umask at checkout. Fixture files are 0644 (0755
            // when executable) and directories 0755 unless the modes manifest says otherwise.
            md.mode = match md.kind {
                super::FileKind::File if md.mode & 0o111 == 0 => 0o644,
                super::FileKind::Symlink => 0o777,
                _ => 0o755,
            };
            if let Some(&(mode, uid, gid)) = self.modes.get(&r.logical) {
                (md.mode, md.uid, md.gid) = (mode, uid, gid);
            }
            // …and their timestamps are the time of the checkout.
            md.mtime = self.mtimes.get(&r.logical).copied().unwrap_or(self.default_mtime);
        }
        if let Some(&(uid, gid)) = self.state().owners.get(&r.logical) {
            (md.uid, md.gid) = (uid, gid);
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
                // `.gitkeep` only exists so git stores an otherwise empty fixture directory
                let placeholder = |name: &String| layer == self.base && name == ".gitkeep";
                names.extend(rooted::dir_names(&real)?.into_iter().filter(|n| !placeholder(n)));
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

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir(self.overlay_only(path)?)
    }

    fn symlink(&self, target: &Path, path: &Path) -> io::Result<()> {
        // Resolve the parent only: the link itself must not be followed.
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a file path: {}", path.display())));
        };
        let placeholder = self.writable(&parent.join(".moi-link"))?;
        rooted::symlink(target, &placeholder.with_file_name(name))
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        let r = self.resolve(path, true)?;
        if self.overlay.is_none() {
            return Err(self.read_only(path));
        }
        if r.real.is_dir() {
            self.state().chmodded.insert(r.logical.clone());
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

    fn set_owner(&self, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
        let r = self.resolve(path, true)?;
        if self.overlay.is_none() || r.layer != Some(0) {
            return self.overlay_only(path).map(|_| ());
        }
        self.state().owners.insert(r.logical, (uid, gid));
        Ok(())
    }

    fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
        // An existing file is wherever it is (the fixture, for read-only opens); a new one goes to the
        // overlay.
        let resolved = self.resolve(path, true)?;
        match (resolved.layer, &self.overlay) {
            (Some(_), _) | (None, None) => Ok(resolved.real),
            (None, Some(_)) => self.writable(path),
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

    fn run_interactive(&self, cmd: &Cmd) -> io::Result<i32> {
        // nobody is at the keyboard: the canned exit code is all there is
        self.run(cmd).map(|out| out.status)
    }

    fn connect_unix(&self, path: &Path) -> io::Result<Box<dyn Conn>> {
        match fake_docker::engine_dir(&self.dir) {
            Some(dir) if path == Path::new(crate::docker::DEFAULT_SOCKET) => {
                Ok(Box::new(FakeConn::new(dir, Arc::clone(&self.docker))))
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, format!("no such socket: {}", path.display()))),
        }
    }

    fn connect_tcp(&self, host: &str, port: u16, _timeout: Duration) -> io::Result<Box<dyn Conn>> {
        match self.http.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            Some(handler) => Ok(Box::new(HandlerConn {
                authority: format!("{host}:{port}"),
                handler,
                request: Vec::new(),
                response: None,
            })),
            None => Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("fixture host has no network: cannot connect to {host}:{port}"),
            )),
        }
    }

    fn random_bytes(&self, count: usize) -> io::Result<Vec<u8>> {
        // not random at all: tests need the same "random" keys every run
        let mut state = self.state();
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            state.random_counter += 1;
            out.extend_from_slice(&Self::command_key(&format!("random {}", state.random_counter)).as_bytes()[..8]);
        }
        out.truncate(count);
        Ok(out)
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
