//! The real machine. With a root prefix other than `/` it only does file access under that prefix (a
//! mounted target during restore, a scratch tree in tests) and refuses to run commands or log, because
//! those would act on the machine moi runs on, not on the tree it was pointed at.

use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use super::rooted;
use super::{Cmd, CmdOutput, Conn, FsUsage, Host, Metadata};

const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";

#[derive(Debug, Clone)]
pub struct RealHost {
    root: PathBuf,
}

impl Default for RealHost {
    fn default() -> Self {
        Self::new()
    }
}

impl RealHost {
    /// The machine itself, rooted at `/`.
    pub fn new() -> Self {
        Self { root: PathBuf::from("/") }
    }

    /// File access confined to `root`.
    pub fn with_root(root: &Path) -> Self {
        Self { root: root.to_path_buf() }
    }

    fn is_system(&self) -> bool {
        self.root == Path::new("/")
    }

    fn real(&self, path: &Path, follow_last: bool) -> io::Result<PathBuf> {
        if self.is_system() {
            if !path.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("path must be absolute: {}", path.display()),
                ));
            }
            return Ok(path.to_path_buf());
        }
        Ok(rooted::resolve(&[&self.root], path, follow_last)?.real)
    }

    fn refuse(&self, what: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("refusing to {what}: this host is confined to {}", self.root.display()),
        )
    }
}

impl Host for RealHost {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(self.real(path, true)?)
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read + Send>> {
        Ok(Box::new(fs::File::open(self.real(path, true)?)?))
    }

    fn create(&self, path: &Path, mode: u32) -> io::Result<Box<dyn io::Write + Send>> {
        use std::os::unix::fs::OpenOptionsExt;
        let file =
            fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(self.real(path, true)?)?;
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))?;
        Ok(Box::new(file))
    }

    fn append(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new().append(true).create(true).mode(mode).open(self.real(path, true)?)?.write_all(data)
    }

    fn lock(&self, path: &Path) -> io::Result<Option<Box<dyn std::any::Any + Send>>> {
        use std::os::unix::fs::OpenOptionsExt;
        let file =
            fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(self.real(path, true)?)?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(Box::new(file))),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        Ok(rooted::to_metadata(&fs::metadata(self.real(path, true)?)?))
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        Ok(rooted::to_metadata(&fs::symlink_metadata(self.real(path, false)?)?))
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        fs::read_link(self.real(path, false)?)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        let mut names = rooted::dir_names(&self.real(path, true)?)?;
        names.sort_unstable();
        Ok(names)
    }

    fn write(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        rooted::write_atomic(&self.real(path, true)?, data, mode)
    }

    fn create_dir_all(&self, path: &Path, mode: u32) -> io::Result<()> {
        rooted::create_dir_all(&self.real(path, true)?, mode)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(self.real(path, false)?)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir(self.real(path, false)?)
    }

    fn symlink(&self, target: &Path, path: &Path) -> io::Result<()> {
        rooted::symlink(target, &self.real(path, false)?)
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        rooted::set_mode(&self.real(path, true)?, mode)
    }

    fn set_owner(&self, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
        std::os::unix::fs::chown(self.real(path, true)?, Some(uid), Some(gid))
    }

    fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
        self.real(path, true)
    }

    fn run(&self, cmd: &Cmd) -> io::Result<CmdOutput> {
        if !self.is_system() {
            return Err(self.refuse(&format!("run `{}`", cmd.line())));
        }
        let mut child = Command::new(&cmd.program)
            .args(&cmd.args)
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(if cmd.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("cannot run `{}`: {e}", cmd.program)))?;

        // Pipes are drained on threads so a chatty child can never block on a full pipe while we wait.
        let stdin = child.stdin.take().zip(cmd.stdin.clone()).map(|(mut pipe, data)| {
            thread::spawn(move || {
                let _ = pipe.write_all(&data);
            })
        });
        let stdout = child.stdout.take().map(drain);
        let stderr = child.stderr.take().map(drain);

        let deadline = cmd.timeout.map(|t| Instant::now() + t);
        let mut timed_out = false;
        let mut pause = Duration::from_millis(1);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                timed_out = true;
                let _ = child.kill();
                break child.wait()?;
            }
            thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(20));
        };
        if let Some(t) = stdin {
            let _ = t.join();
        }
        let collect = |t: Option<thread::JoinHandle<Vec<u8>>>| t.and_then(|t| t.join().ok()).unwrap_or_default();
        Ok(CmdOutput {
            status: status.code().or_else(|| status.signal().map(|s| 128 + s)).unwrap_or(-1),
            stdout: collect(stdout),
            stderr: collect(stderr),
            timed_out,
        })
    }

    fn run_interactive(&self, cmd: &Cmd) -> io::Result<i32> {
        if !self.is_system() {
            return Err(self.refuse(&format!("run `{}`", cmd.line())));
        }
        let status = Command::new(&cmd.program).args(&cmd.args).envs(cmd.env.iter().map(|(k, v)| (k, v))).status()?;
        Ok(status.code().or_else(|| status.signal().map(|s| 128 + s)).unwrap_or(-1))
    }

    fn connect_unix(&self, path: &Path) -> io::Result<Box<dyn Conn>> {
        if !self.is_system() {
            return Err(self.refuse(&format!("connect to {}", path.display())));
        }
        Ok(Box::new(UnixStream::connect(path)?))
    }

    fn ask(&self, prompt: &str, timeout: Duration) -> io::Result<Option<String>> {
        use std::io::{BufRead, Write};
        if !self.is_system() {
            return Err(self.refuse("ask at the terminal"));
        }
        let mut stderr = io::stderr();
        write!(stderr, "{prompt}")?;
        stderr.flush()?;
        // reading a line cannot be interrupted: read on a thread and stop waiting
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new().name("moi-ask".into()).spawn(move || {
            let mut line = String::new();
            let read = io::stdin().lock().read_line(&mut line);
            let _ = tx.send(read.ok().filter(|n| *n > 0).map(|_| line.trim().to_string()));
        })?;
        Ok(rx.recv_timeout(timeout).ok().flatten())
    }

    fn watch(&self, dirs: &[PathBuf]) -> io::Result<Box<dyn super::Watch>> {
        // like file access, watching works inside a confined root: paths are mapped in and back out
        inotify::watch(self.root.clone(), dirs)
    }

    fn listen_unix(&self, path: &Path, mode: u32) -> io::Result<Box<dyn super::Listener>> {
        if !self.is_system() {
            return Err(self.refuse(&format!("listen on {}", path.display())));
        }
        bind_unix(path, mode)
    }

    fn notify(&self, state: &str) -> io::Result<bool> {
        let Some(target) = std::env::var_os("NOTIFY_SOCKET").filter(|t| !t.is_empty()) else { return Ok(false) };
        if !self.is_system() {
            return Err(self.refuse("notify the service manager"));
        }
        let socket = std::os::unix::net::UnixDatagram::unbound()?;
        let name = target.to_string_lossy();
        match name.strip_prefix('@') {
            // an abstract socket (no file): Linux only
            Some(_abstract_name) => {
                #[cfg(target_os = "linux")]
                {
                    use std::os::linux::net::SocketAddrExt;
                    let address = std::os::unix::net::SocketAddr::from_abstract_name(_abstract_name.as_bytes())?;
                    socket.send_to_addr(state.as_bytes(), &address)?;
                }
                #[cfg(not(target_os = "linux"))]
                return Err(io::Error::new(io::ErrorKind::Unsupported, "abstract notify sockets exist on Linux only"));
            }
            None => {
                socket.send_to(state.as_bytes(), Path::new(&target))?;
            }
        }
        Ok(true)
    }

    fn statvfs(&self, path: &Path) -> io::Result<FsUsage> {
        let stat = rustix::fs::statvfs(self.real(path, true)?)?;
        Ok(FsUsage {
            blocks: stat.f_blocks,
            blocks_free: stat.f_bfree,
            blocks_available: stat.f_bavail,
            inodes: stat.f_files,
            inodes_free: stat.f_ffree,
        })
    }

    fn connect_tcp(&self, host: &str, port: u16, timeout: Duration) -> io::Result<Box<dyn Conn>> {
        if !self.is_system() {
            return Err(self.refuse(&format!("connect to {host}:{port}")));
        }
        let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} does not resolve"));
        for address in (host, port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&address, timeout) {
                Ok(stream) => return Ok(Box::new(stream)),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn resolve(&self, name: &str, timeout: Duration) -> io::Result<Vec<std::net::IpAddr>> {
        if let Ok(ip) = name.parse() {
            return Ok(vec![ip]);
        }
        if !self.is_system() {
            return Err(self.refuse(&format!("resolve {name}")));
        }
        // the system resolver has no timeout of its own: ask on a thread and stop waiting
        let (tx, rx) = std::sync::mpsc::channel();
        let query = name.to_string();
        std::thread::Builder::new().name("moi-resolve".into()).spawn(move || {
            let _ =
                tx.send((query.as_str(), 0).to_socket_addrs().map(|found| found.map(|a| a.ip()).collect::<Vec<_>>()));
        })?;
        match rx.recv_timeout(timeout) {
            Ok(Ok(found)) if found.is_empty() => {
                Err(io::Error::new(io::ErrorKind::NotFound, format!("{name} does not resolve")))
            }
            Ok(found) => found,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, format!("resolving {name} timed out"))),
        }
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn monotonic(&self) -> Duration {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed()
    }

    fn random_bytes(&self, count: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; count];
        fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn journal(&self, fields: &[(String, String)]) -> io::Result<()> {
        if !self.is_system() {
            return Err(self.refuse("write to the journal"));
        }
        let socket = std::os::unix::net::UnixDatagram::unbound()?;
        socket.send_to(&crate::log::encode_journal(fields), JOURNAL_SOCKET)?;
        Ok(())
    }
}

/// Bind a unix stream socket, replacing a stale socket file.
pub(crate) fn bind_unix(real: &Path, mode: u32) -> io::Result<Box<dyn super::Listener>> {
    match fs::symlink_metadata(real) {
        Ok(existing) if std::os::unix::fs::FileTypeExt::is_socket(&existing.file_type()) => fs::remove_file(real)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", real.display()),
            ));
        }
        Err(_) => {}
    }
    let listener = std::os::unix::net::UnixListener::bind(real)?;
    fs::set_permissions(real, std::os::unix::fs::PermissionsExt::from_mode(mode))?;
    Ok(Box::new(listener))
}

impl super::Listener for std::os::unix::net::UnixListener {
    fn accept(&self) -> io::Result<Box<dyn Conn>> {
        let (stream, _) = std::os::unix::net::UnixListener::accept(self)?;
        Ok(Box::new(stream))
    }
}

impl Conn for TcpStream {
    fn set_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }
}

impl Conn for UnixStream {
    fn set_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

#[cfg(target_os = "linux")]
mod inotify {
    use std::collections::{BTreeMap, VecDeque};
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::OwnedFd;
    use std::path::{Path, PathBuf};

    use rustix::fs::inotify::{self, CreateFlags, Reader, WatchFlags};

    #[derive(Debug)]
    struct Inotify {
        fd: OwnedFd,
        /// The host's root on this machine (`/` for the machine itself).
        root: PathBuf,
        /// Watch descriptor → the directory it watches, as the host names it.
        dirs: BTreeMap<i32, PathBuf>,
        /// Changes already read from the kernel and not handed out yet.
        queue: VecDeque<PathBuf>,
    }

    fn changes() -> WatchFlags {
        WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::ATTRIB
            | WatchFlags::DELETE_SELF
    }

    impl crate::host::Watch for Inotify {
        fn add(&mut self, dir: &Path) -> io::Result<()> {
            let real = self.root.join(dir.strip_prefix("/").unwrap_or(dir));
            let wd = inotify::add_watch(&self.fd, &real, changes() | WatchFlags::ONLYDIR)?;
            self.dirs.insert(wd, dir.to_path_buf());
            Ok(())
        }

        fn next(&mut self) -> io::Result<PathBuf> {
            loop {
                if let Some(path) = self.queue.pop_front() {
                    return Ok(path);
                }
                // One read hands over every event the kernel has ready, as many as fit: all of them
                // must be taken out of the buffer before it is dropped.
                let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
                let mut reader = Reader::new(&self.fd, &mut buffer);
                let mut read: Vec<(i32, Option<String>)> = Vec::new();
                loop {
                    let event = reader.next()?;
                    read.push((event.wd(), event.file_name().map(|name| name.to_string_lossy().into_owned())));
                    if reader.is_buffer_empty() {
                        break;
                    }
                }
                for (wd, name) in read {
                    if let Some(dir) = self.dirs.get(&wd) {
                        self.queue.push_back(name.map_or_else(|| dir.clone(), |name| dir.join(name)));
                    }
                }
            }
        }
    }

    pub(super) fn watch(root: PathBuf, dirs: &[PathBuf]) -> io::Result<Box<dyn crate::host::Watch>> {
        let mut watch =
            Inotify { fd: inotify::init(CreateFlags::CLOEXEC)?, root, dirs: BTreeMap::new(), queue: VecDeque::new() };
        for dir in dirs {
            // a directory that is not there (or not a directory) is simply not watched
            let _ = crate::host::Watch::add(&mut watch, dir);
        }
        Ok(Box::new(watch))
    }
}

#[cfg(not(target_os = "linux"))]
mod inotify {
    use std::io;
    use std::path::PathBuf;

    pub(super) fn watch(_root: PathBuf, _dirs: &[PathBuf]) -> io::Result<Box<dyn crate::host::Watch>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "watching files needs inotify (Linux)"))
    }
}
