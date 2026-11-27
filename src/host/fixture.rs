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

use super::fake_docker::{DockerState, FakeConn};
use super::fake_net::{self, Net};
use super::rooted::{self, Resolved};
use super::{Cmd, CmdOutput, Conn, FsUsage, Host, Metadata};

/// 2026-01-01T00:00:00Z, used when the fixture has no `clock` file.
const DEFAULT_CLOCK: u64 = 1_767_225_600;

/// The fixture's whole network: gets `(host:port, raw HTTP request)` and returns the raw response.
pub type HttpHandler = Arc<dyn Fn(&str, &[u8]) -> Vec<u8> + Send + Sync>;

pub struct FixtureHost {
    dir: PathBuf,
    /// A case directory layered over `dir` (a parity case): same layout, every part optional.
    case: Option<PathBuf>,
    base: PathBuf,
    overlay: Option<PathBuf>,
    modes: BTreeMap<PathBuf, (u32, u32, u32)>,
    mtimes: BTreeMap<PathBuf, SystemTime>,
    /// Modification time of every fixture file not listed in `mtimes`: 30 days before the clock.
    default_mtime: SystemTime,
    /// Filesystem usage by mount point, from `statvfs` files.
    usage: BTreeMap<PathBuf, FsUsage>,
    /// Mount points whose `statvfs` fails (`/mount error` in a `statvfs` file).
    usage_errors: Vec<PathBuf>,
    /// Paths that do not exist in this case although the fixture host has them (`absent` file).
    absent: Vec<PathBuf>,
    /// Paths below which nothing can be written (`unwritable` file).
    unwritable: Vec<PathBuf>,
    state: Mutex<State>,
    docker: Arc<Mutex<DockerState>>,
    http: Mutex<Option<HttpHandler>>,
    /// The fake network described by `net` files.
    net: Arc<Net>,
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
    /// What whoever is "at the terminal" will type, in order, and what they were asked.
    answers: Vec<String>,
    asked: Vec<String>,
    /// Standard input of commands that were given one: (command line, input).
    inputs: Vec<(String, String)>,
    /// Watches handed out: the directories each covers, and where its events go.
    watchers: Vec<(Vec<PathBuf>, std::sync::mpsc::Sender<PathBuf>)>,
    /// What was sent to the service manager (`READY=1`, …).
    notified: Vec<String>,
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

    fn fake_tls(&mut self) -> Option<io::Result<()>> {
        Some(Ok(()))
    }
}

/// A watch whose events the test sends.
#[derive(Debug)]
struct FixtureWatch {
    rx: std::sync::mpsc::Receiver<PathBuf>,
}

impl super::Watch for FixtureWatch {
    fn add(&mut self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }

    fn next(&mut self) -> io::Result<PathBuf> {
        self.rx.recv().map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the fixture host is gone"))
    }
}

/// The fixed "now" of a fixture or case directory, if it has a `clock` file.
fn read_clock(dir: &Path) -> io::Result<Option<SystemTime>> {
    match fs::read_to_string(dir.join("clock")) {
        Ok(text) => super::parse_rfc3339(&text)
            .map(Some)
            .ok_or_else(|| bad_fixture(format!("{}/clock is not an RFC 3339 time: {}", dir.display(), text.trim()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
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
        // what an earlier fixture on this thread waited is not this one's time
        fake_net::reset();
        let now = read_clock(dir)?.unwrap_or(UNIX_EPOCH + Duration::from_secs(DEFAULT_CLOCK));
        let mut host = Self {
            dir: dir.to_path_buf(),
            case: None,
            base,
            overlay: None,
            modes: BTreeMap::new(),
            mtimes: BTreeMap::new(),
            default_mtime: now - Duration::from_secs(30 * 86_400),
            usage: BTreeMap::new(),
            usage_errors: Vec::new(),
            absent: Vec::new(),
            unwritable: Vec::new(),
            state: Mutex::new(State {
                now,
                env: BTreeMap::new(),
                commands: BTreeMap::new(),
                ran: Vec::new(),
                journal: Vec::new(),
                owners: BTreeMap::new(),
                chmodded: BTreeSet::new(),
                answers: Vec::new(),
                asked: Vec::new(),
                inputs: Vec::new(),
                watchers: Vec::new(),
                notified: Vec::new(),
                random_counter: 0,
            }),
            http: Mutex::new(None),
            net: Arc::default(),
            docker: Arc::default(),
        };
        host.read_manifests(dir)?;
        Ok(host)
    }

    /// Layer a case directory over this fixture host: a directory with the same layout in which every
    /// part is optional. Its `root/`, `commands/` and `docker/` files shadow the host's; its `modes`,
    /// `mtimes` and `statvfs` lines override the host's; its `clock` replaces the host's; `absent`
    /// lists host paths that do not exist in this case and `unwritable` paths that cannot be written.
    pub fn with_case(mut self, case: &Path) -> io::Result<Self> {
        if !case.is_dir() {
            return Err(bad_fixture(format!("{} is not a directory", case.display())));
        }
        if let Some(now) = read_clock(case)? {
            self.state().now = now;
            self.default_mtime = now - Duration::from_secs(30 * 86_400);
        }
        self.read_manifests(case)?;
        self.case = Some(case.to_path_buf());
        Ok(self)
    }

    /// `modes`, `mtimes`, `statvfs`, `absent` and `unwritable` of a fixture or case directory.
    fn read_manifests(&mut self, dir: &Path) -> io::Result<()> {
        let lines = |name: &str| -> io::Result<Vec<(usize, String)>> {
            match fs::read_to_string(dir.join(name)) {
                Ok(text) => Ok(text
                    .lines()
                    .enumerate()
                    .map(|(n, l)| (n + 1, l.trim().to_string()))
                    .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
                    .collect()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(e) => Err(e),
            }
        };
        let bad = |name: &str, n: usize, expected: &str| {
            bad_fixture(format!("{}/{name}:{n}: expected {expected}", dir.display()))
        };
        for (n, line) in lines("modes")? {
            let mut parts = line.splitn(4, char::is_whitespace);
            let parsed = (|| {
                let mode = u32::from_str_radix(parts.next()?, 8).ok()?;
                let uid = parts.next()?.parse().ok()?;
                let gid = parts.next()?.parse().ok()?;
                let path = parts.next()?.trim();
                path.starts_with('/').then(|| (PathBuf::from(path), (mode, uid, gid)))
            })();
            let (path, entry) = parsed.ok_or_else(|| bad("modes", n, "`mode uid gid /path`"))?;
            self.modes.insert(path, entry);
        }
        for (n, line) in lines("mtimes")? {
            let parsed = line
                .split_once(' ')
                .and_then(|(time, path)| Some((PathBuf::from(path.trim()), super::parse_rfc3339(time)?)));
            let (path, time) = parsed.ok_or_else(|| bad("mtimes", n, "`<RFC 3339 time> /path`"))?;
            self.mtimes.insert(path, time);
        }
        for (n, line) in lines("statvfs")? {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let numbers: Option<Vec<u64>> = fields.iter().skip(1).map(|f| f.parse().ok()).collect();
            // a case can take a filesystem away (`/mnt/usb -`) or make asking about it fail (`/mnt/usb error`)
            if let [path, marker] = fields.as_slice()
                && path.starts_with('/')
            {
                match *marker {
                    "-" => {
                        self.usage.remove(Path::new(path));
                        continue;
                    }
                    "error" => {
                        self.usage_errors.push(PathBuf::from(path));
                        continue;
                    }
                    _ => {}
                }
            }
            match (fields.first(), numbers.as_deref()) {
                (Some(path), Some([blocks, free, available, inodes, inodes_free])) if path.starts_with('/') => {
                    let usage = FsUsage {
                        blocks: *blocks,
                        blocks_free: *free,
                        blocks_available: *available,
                        inodes: *inodes,
                        inodes_free: *inodes_free,
                    };
                    self.usage.insert(PathBuf::from(path), usage);
                }
                _ => return Err(bad("statvfs", n, "`/mount blocks free available inodes inodes-free`")),
            }
        }
        for (n, line) in lines("net")? {
            Arc::make_mut(&mut self.net).add_line(&line).map_err(|expected| bad("net", n, expected))?;
        }
        self.absent.extend(lines("absent")?.into_iter().map(|(_, l)| PathBuf::from(l)));
        self.unwritable.extend(lines("unwritable")?.into_iter().map(|(_, l)| PathBuf::from(l)));
        Ok(())
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

    /// What the person at the terminal answers to the next question (`Host::ask`); without one, they
    /// say nothing and the question times out.
    pub fn answer(&self, text: &str) {
        self.state().answers.push(text.to_string());
    }

    /// Every question asked at the terminal so far.
    pub fn asked(&self) -> Vec<String> {
        self.state().asked.clone()
    }

    /// What commands were given on standard input: (command line, input).
    pub fn inputs(&self) -> Vec<(String, String)> {
        self.state().inputs.clone()
    }

    /// Tell every watch that covers this path's directory that the path changed (the fixture's inotify).
    /// Returns how many watches heard it.
    pub fn file_changed(&self, path: &Path) -> usize {
        let parent = path.parent().unwrap_or(Path::new("/"));
        let state = self.state();
        state
            .watchers
            .iter()
            .filter(|(dirs, _)| dirs.iter().any(|d| d == parent))
            .filter(|(_, tx)| tx.send(path.to_path_buf()).is_ok())
            .count()
    }

    /// Every directory some watch covers.
    pub fn watched(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self.state().watchers.iter().flat_map(|(dirs, _)| dirs.clone()).collect();
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// Everything sent to the service manager so far.
    pub fn notifications(&self) -> Vec<String> {
        self.state().notified.clone()
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

    /// The read-only roots, most specific first: the case's, then the fixture host's.
    fn fixture_roots(&self) -> Vec<PathBuf> {
        let case_root = self.case.as_ref().map(|c| c.join("root")).filter(|r| r.is_dir());
        case_root.into_iter().chain(std::iter::once(self.base.clone())).collect()
    }

    /// Every root a path is looked up in: the writable overlay first, then the fixture roots.
    fn layers(&self) -> Vec<PathBuf> {
        self.overlay.iter().cloned().chain(self.fixture_roots()).collect()
    }

    /// Where a path is, whether or not the case declares it absent (for creating it again).
    fn locate(&self, path: &Path, follow_last: bool) -> io::Result<Resolved> {
        let layers = self.layers();
        let layers: Vec<&Path> = layers.iter().map(PathBuf::as_path).collect();
        rooted::resolve(&layers, path, follow_last)
    }

    fn resolve(&self, path: &Path, follow_last: bool) -> io::Result<Resolved> {
        let resolved = self.locate(path, follow_last)?;
        // a path the case declares absent is gone unless the test created it again in the overlay
        let in_overlay = self.overlay.is_some() && resolved.layer == Some(0);
        if !in_overlay && self.absent.iter().any(|gone| resolved.logical.starts_with(gone)) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such file or directory: {}", path.display()),
            ));
        }
        Ok(resolved)
    }

    /// The `commands/` or `docker/` directories of the case and the host, most specific first.
    fn fixture_dirs(&self, name: &str) -> Vec<PathBuf> {
        self.case.iter().chain(std::iter::once(&self.dir)).map(|d| d.join(name)).filter(|d| d.is_dir()).collect()
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
            && self.fixture_roots().iter().any(|root| root.join(relative).is_dir())
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
        // something absent in this case can be created again
        let r = self.locate(path, true)?;
        if self.unwritable.iter().any(|denied| r.logical.starts_with(denied)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("read-only file system: {}", path.display()),
            ));
        }
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
        // what the case declares absent is only there as far as the overlay has it again
        let gone = self.absent.iter().any(|gone| logical.starts_with(gone));
        for layer in self.layers() {
            let real = layer.join(relative);
            if real.is_dir() && (!gone || Some(&layer) == self.overlay.as_ref()) {
                found = true;
                // `.gitkeep` only exists so git stores an otherwise empty fixture directory
                let placeholder = |name: &String| Some(&layer) != self.overlay.as_ref() && name == ".gitkeep";
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
        let logical = self.locate(path, true)?.logical;
        let relative = logical.strip_prefix("/").unwrap_or(&logical);
        // Parents that only exist in the fixture are mirrored with ordinary permissions; `mode` is for
        // the directories this call really adds to the host.
        let mut existing = relative;
        let roots = self.fixture_roots();
        while !existing.as_os_str().is_empty() && !roots.iter().any(|root| root.join(existing).is_dir()) {
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
        if let Some(input) = &cmd.stdin {
            state.inputs.push((line.clone(), String::from_utf8_lossy(input).into_owned()));
        }
        if let Some(out) = state.commands.get(&line) {
            return Ok(out.clone());
        }
        drop(state);
        let key = Self::command_key(&line);
        let dirs = self.fixture_dirs("commands");
        let missing = || {
            let add = self.dir.join("commands").join(format!("{key}.out"));
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no canned output for `{line}` in this fixture: add {}", add.display()),
            )
        };
        // the most specific directory that knows the command answers; `<key>.missing` there means the
        // tool is not installed in this case, `<key>.timeout` that it never finishes
        let known =
            |d: &&PathBuf| ["out", "missing", "timeout"].iter().any(|ext| d.join(format!("{key}.{ext}")).exists());
        let Some(dir) = dirs.iter().find(known) else { return Err(missing()) };
        let file = |ext: &str| dir.join(format!("{key}.{ext}"));
        if file("missing").exists() {
            return Err(missing());
        }
        if file("timeout").exists() {
            return Ok(CmdOutput { status: -1, stdout: Vec::new(), stderr: Vec::new(), timed_out: true });
        }
        let stdout = fs::read(file("out"))?;
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
        if path != Path::new(crate::docker::DEFAULT_SOCKET) {
            // a socket something listens on in the overlay (the daemon under test)
            let resolved = self.resolve(path, true)?;
            return match resolved.layer {
                Some(0) if self.overlay.is_some() => {
                    Ok(Box::new(std::os::unix::net::UnixStream::connect(resolved.real)?))
                }
                _ => Err(io::Error::new(io::ErrorKind::NotFound, format!("no such socket: {}", path.display()))),
            };
        }
        let dirs = self.fixture_dirs("docker");
        if dirs.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("no such socket: {}", path.display())));
        }
        Ok(Box::new(FakeConn::new(dirs, Arc::clone(&self.docker))))
    }

    fn ask(&self, prompt: &str, timeout: Duration) -> io::Result<Option<String>> {
        let mut state = self.state();
        state.asked.push(prompt.to_string());
        let answer = if state.answers.is_empty() { None } else { Some(state.answers.remove(0)) };
        drop(state);
        if answer.is_none() {
            // nobody answered: the time to answer passes
            fake_net::spend(timeout);
        }
        Ok(answer)
    }

    fn watch(&self, dirs: &[PathBuf]) -> io::Result<Box<dyn super::Watch>> {
        // fed by the test: `file_changed`
        let (tx, rx) = std::sync::mpsc::channel();
        self.state().watchers.push((dirs.to_vec(), tx));
        Ok(Box::new(FixtureWatch { rx }))
    }

    fn listen_unix(&self, path: &Path, mode: u32) -> io::Result<Box<dyn super::Listener>> {
        // a real socket, in the overlay
        if let Some(parent) = path.parent() {
            self.create_dir_all(parent, 0o755)?;
        }
        super::real::bind_unix(&self.writable(path)?, mode)
    }

    fn notify(&self, state: &str) -> io::Result<bool> {
        let mut inner = self.state();
        // as under systemd: only a service started with a notify socket is listened to
        if !inner.env.contains_key("NOTIFY_SOCKET") {
            return Ok(false);
        }
        inner.notified.push(state.to_string());
        Ok(true)
    }

    fn statvfs(&self, path: &Path) -> io::Result<FsUsage> {
        let logical = self.resolve(path, true)?.logical;
        if self.usage_errors.iter().any(|mount| logical.starts_with(mount)) {
            return Err(io::Error::other(format!("statvfs fails for {} in this fixture", path.display())));
        }
        // the filesystem a path is on: the longest mount point listed that contains it
        self.usage
            .iter()
            .filter(|(mount, _)| logical.starts_with(mount))
            .max_by_key(|(mount, _)| mount.as_os_str().len())
            .map(|(_, usage)| *usage)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no statvfs line covers {} in this fixture", path.display()),
                )
            })
    }

    fn connect_tcp(&self, host: &str, port: u16, timeout: Duration) -> io::Result<Box<dyn Conn>> {
        match self.http.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            Some(handler) => Ok(Box::new(HandlerConn {
                authority: format!("{host}:{port}"),
                handler,
                request: Vec::new(),
                response: None,
            })),
            None => self.net.connect(host, port, timeout),
        }
    }

    fn resolve(&self, name: &str, timeout: Duration) -> io::Result<Vec<std::net::IpAddr>> {
        self.net.resolve(name, timeout)
    }

    fn sleep(&self, duration: Duration) {
        fake_net::spend(duration);
    }

    fn monotonic(&self) -> Duration {
        self.now().duration_since(UNIX_EPOCH).unwrap_or_default()
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
        // the fixed clock plus what this thread has "waited" (sleeps, fake network latency)
        self.state().now + fake_net::spent()
    }

    fn env(&self, name: &str) -> Option<String> {
        self.state().env.get(name).cloned()
    }

    fn journal(&self, fields: &[(String, String)]) -> io::Result<()> {
        self.state().journal.push(fields.to_vec());
        Ok(())
    }
}
