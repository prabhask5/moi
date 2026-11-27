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
