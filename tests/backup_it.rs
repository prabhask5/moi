//! `moi backup`, `--drill`, `push` and `pull` against the real things (CI integration job only): the
//! runner's Docker engine with its Postgres service container, and a real restic with a local
//! repository.
//!
//! The machine moi sees is a sandbox: a copy of the cestmoi fixture's root in a scratch directory,
//! reached through a confined `RealHost`. Two doors lead out of it, both opened here and nowhere else:
//! the Docker socket (so dumps and drill containers are real), and `restic`, which runs chrooted into
//! the sandbox (so the paths it stores and restores are the paths moi names).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cestmoi::cli;
use cestmoi::docker::{Docker, ExecOptions};
use cestmoi::host::{Cmd, CmdOutput, Conn, FsUsage, Host, Listener, Metadata, RealHost, Watch};

#[derive(Debug)]
struct Sandbox {
    root: PathBuf,
    inner: RealHost,
    env: Mutex<BTreeMap<String, String>>,
    /// Every restic invocation, for the assertions.
    restic: Mutex<Vec<String>>,
}

fn sudo(args: &[&str]) -> std::process::Output {
    Command::new("sudo").args(args).output().expect("sudo")
}

impl Sandbox {
    /// A fresh sandbox holding the cestmoi fixture's files, a restic binary and room for a repository.
    fn new(name: &str) -> Arc<Self> {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        // an earlier run left files that belong to root (restic ran as root)
        sudo(&["rm", "-rf", root.to_str().unwrap()]);
        std::fs::create_dir_all(&root).unwrap();
        let fixture = common::fixture_dir("cestmoi").join("root");
        assert!(
            Command::new("cp")
                .args(["-a", &format!("{}/.", fixture.display()), root.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        for dir in ["run", "tmp", "repo", "var/lib/moi"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let restic =
            String::from_utf8(Command::new("sh").args(["-c", "command -v restic"]).output().unwrap().stdout).unwrap();
        std::fs::copy(restic.trim(), root.join("restic")).expect("restic is installed in the integration job");
        // a dynamically linked restic needs its loader and libraries at the same paths inside
        let linked = Command::new("ldd")
            .arg(restic.trim())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        for library in linked.split_whitespace().filter(|word| word.starts_with('/')) {
            let inside = root.join(library.trim_start_matches('/'));
            std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
            std::fs::copy(library, &inside).unwrap_or_else(|e| panic!("{library}: {e}"));
        }
        Arc::new(Self { inner: RealHost::with_root(&root), root, env: Mutex::default(), restic: Mutex::default() })
    }

    fn set_env(&self, key: &str, value: &str) {
        self.env.lock().unwrap().insert(key.to_string(), value.to_string());
    }

    fn write(&self, path: &str, text: &str, mode: u32) {
        self.inner.create_dir_all(Path::new(path).parent().unwrap(), 0o755).unwrap();
        Host::write(&self.inner, Path::new(path), text.as_bytes(), mode).unwrap();
    }

    fn moi(self: &Arc<Self>, args: &[&str]) -> (u8, String) {
        let host: Arc<dyn Host> = self.clone();
        let mut out = Vec::new();
        match cli::execute(&cli::parse(args).unwrap(), host, &mut out) {
            Ok(()) => (0, String::from_utf8(out).unwrap()),
            Err(e) if e.exit_code() == 1 => (1, String::from_utf8(out).unwrap()),
            Err(e) => (e.exit_code(), e.to_string()),
        }
    }

    /// restic inside the sandbox, as root, with this environment.
    fn restic(&self, args: &[String], env: &[(String, String)]) -> io::Result<CmdOutput> {
        self.restic.lock().unwrap().push(args.join(" "));
        let mut command = Command::new("sudo");
        command.arg("env").arg("RESTIC_CACHE_DIR=/tmp/restic-cache").arg("HOME=/tmp");
        for (key, value) in
            env.iter().cloned().chain(self.env.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())))
        {
            command.arg(format!("{key}={value}"));
        }
        let out = command.arg("chroot").arg(&self.root).arg("/restic").args(args).output()?;
        Ok(CmdOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
            timed_out: false,
        })
    }
}

impl Host for Sandbox {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read + Send>> {
        self.inner.open(path)
    }
    fn create(&self, path: &Path, mode: u32) -> io::Result<Box<dyn io::Write + Send>> {
        self.inner.create(path, mode)
    }
    fn append(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        self.inner.append(path, data, mode)
    }
    fn lock(&self, path: &Path) -> io::Result<Option<Box<dyn std::any::Any + Send>>> {
        self.inner.lock(path)
    }
    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.inner.metadata(path)
    }
    fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.inner.symlink_metadata(path)
    }
    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.read_link(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        self.inner.read_dir(path)
    }
    fn write(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        self.inner.write(path, data, mode)
    }
    fn create_dir_all(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.inner.create_dir_all(path, mode)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir(path)
    }
    fn symlink(&self, target: &Path, path: &Path) -> io::Result<()> {
        self.inner.symlink(target, path)
    }
    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.inner.set_mode(path, mode)
    }
    fn set_owner(&self, _path: &Path, _uid: u32, _gid: u32) -> io::Result<()> {
        // the test does not run as root: ownership inside the sandbox stays the runner's
        Ok(())
    }
    fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.real_path(path)
    }
    fn run(&self, cmd: &Cmd) -> io::Result<CmdOutput> {
        if cmd.program == "restic" {
            return self.restic(&cmd.args, &cmd.env);
        }
        // nothing else of the runner is the sandbox's business: every other tool is "not installed"
        Err(io::Error::new(io::ErrorKind::NotFound, format!("{} is not in the sandbox", cmd.program)))
    }
    fn run_interactive(&self, cmd: &Cmd) -> io::Result<i32> {
        self.run(cmd).map(|out| out.status)
    }
    fn connect_unix(&self, path: &Path) -> io::Result<Box<dyn Conn>> {
        if path == Path::new("/var/run/docker.sock") {
            return Ok(Box::new(std::os::unix::net::UnixStream::connect(path)?));
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "no such socket in the sandbox"))
    }
    fn ask(&self, _prompt: &str, _timeout: Duration) -> io::Result<Option<String>> {
        Ok(None)
    }
    fn watch(&self, dirs: &[PathBuf]) -> io::Result<Box<dyn Watch>> {
        self.inner.watch(dirs)
    }
    fn listen_unix(&self, _path: &Path, _mode: u32) -> io::Result<Box<dyn Listener>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no listening in the sandbox"))
    }
    fn notify(&self, _state: &str) -> io::Result<bool> {
        Ok(false)
    }
    fn statvfs(&self, path: &Path) -> io::Result<FsUsage> {
        self.inner.statvfs(path)
    }
    fn connect_tcp(&self, host: &str, port: u16, _timeout: Duration) -> io::Result<Box<dyn Conn>> {
        Err(io::Error::new(io::ErrorKind::ConnectionRefused, format!("the sandbox has no network ({host}:{port})")))
    }
    fn resolve(&self, name: &str, _timeout: Duration) -> io::Result<Vec<std::net::IpAddr>> {
        Err(io::Error::new(io::ErrorKind::NotFound, format!("{name} does not resolve in the sandbox")))
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
    fn monotonic(&self) -> Duration {
        self.inner.monotonic()
    }
    fn random_bytes(&self, count: usize) -> io::Result<Vec<u8>> {
        self.inner.random_bytes(count)
    }
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn env(&self, name: &str) -> Option<String> {
        self.env.lock().unwrap().get(name).cloned()
    }
    fn journal(&self, _fields: &[(String, String)]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no journal in the sandbox"))
    }
}

const PASSWORD: &str = "FAKE-restic-password-for-CI";
const CONFIG: &str = r#"[server]
name = "moi-it"

[backup]
local_dir = "/mnt/storage/backups"
require_mount = "/mnt/storage"
paths = ["/opt/scripts", "/opt/services", "/etc/moi"]
exclude = ["*/cache"]

[backup.offsite]
repository = "/repo"
password = "secret:restic_password"
keep = { daily = 7, weekly = 4, monthly = 6 }
"#;

/// Every file under a directory with its content, for comparing two trees.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())) {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                out.insert(
                    entry.path().strip_prefix(dir).unwrap().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    out
}

#[test]
#[ignore = "needs MOI_IT services (Docker with a Postgres container, restic, sudo)"]
fn backup_drill_push_and_pull_with_a_real_database_and_a_real_repository() {
    let s = Sandbox::new("it-backup");
    s.write("/etc/moi/moi.toml", CONFIG, 0o644);
    s.write("/etc/moi/secrets.toml", &format!("restic_password = \"{PASSWORD}\"\n"), 0o600);
    let host: Arc<dyn Host> = s.clone();

    // ---- a database with something in it: the job's Postgres service container
    let docker = Docker::new(host.clone());
    let postgres = docker
        .containers_inspect()
        .unwrap()
        .into_iter()
        .find(|c| c["Config"]["Image"].as_str().is_some_and(|i| i.starts_with("postgres:")))
        .expect("the integration job runs a postgres service container");
    let (id, name) = (
        postgres["Id"].as_str().unwrap().to_string(),
        postgres["Name"].as_str().unwrap().trim_start_matches('/').to_string(),
    );
    let sql = "CREATE TABLE IF NOT EXISTS moi_it_marker (id integer PRIMARY KEY, note text); INSERT INTO moi_it_marker VALUES (1, 'written before the backup') ON CONFLICT DO NOTHING;";
    let created = docker.exec(&id, &["psql", "-U", "postgres", "-c", sql], &ExecOptions::default()).unwrap();
    assert!(created.success(), "{}", String::from_utf8_lossy(&created.stderr));

    // ---- a repository, as its owner would have made it
    let init = s
        .restic(
            &["init".to_string()],
            &[("RESTIC_REPOSITORY".into(), "/repo".into()), ("RESTIC_PASSWORD".into(), PASSWORD.into())],
        )
        .unwrap();
    assert_eq!(init.status, 0, "{}", String::from_utf8_lossy(&init.stderr));

    // ---- the plan sees the real database
    let (code, plan) = s.moi(&["backup", "--dry-run"]);
    assert_eq!(code, 0, "{plan}");
    assert!(
        plan.lines().any(|l| l.contains(&name) && l.contains("postgres") && l.ends_with("user postgres")),
        "{plan}"
    );

    // ---- the backup
    let (code, out) = s.moi(&["backup"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(&format!("Dumping Postgres: {name} (user postgres)")), "{out}");
    assert!(
        out.contains("offsite: uploading ")
            && out.contains("offsite: state bundle saved")
            && out.contains("offsite: done"),
        "{out}"
    );
    let run = out.lines().last().unwrap().split("Backup OK: ").nth(1).expect("the run's directory").to_string();
    // the dump is a real pg_dumpall, compressed, and has the row
    let mut dump = String::new();
    io::Read::read_to_string(
        &mut flate2::read::GzDecoder::new(host.open(Path::new(&format!("{run}/db/{name}.sql.gz"))).unwrap()),
        &mut dump,
    )
    .unwrap();
    assert!(
        dump.contains("PostgreSQL database cluster dump")
            && dump.contains("moi_it_marker")
            && dump.contains("written before the backup"),
        "{}",
        &dump[..dump.len().min(400)]
    );

    // ---- what the repository holds, asked of restic itself
    let env =
        [("RESTIC_REPOSITORY".to_string(), "/repo".to_string()), ("RESTIC_PASSWORD".to_string(), PASSWORD.to_string())];
    let snapshots = |tag: &str| -> Vec<serde_json::Value> {
        let listed = s.restic(&["snapshots".into(), "--tag".into(), tag.into(), "--json".into()], &env).unwrap();
        assert_eq!(listed.status, 0, "{}", String::from_utf8_lossy(&listed.stderr));
        serde_json::from_slice(&listed.stdout).unwrap()
    };
    let nightly = snapshots("nightly");
    assert_eq!(nightly.len(), 1);
    let paths: Vec<&str> = nightly[0]["paths"].as_array().unwrap().iter().filter_map(|p| p.as_str()).collect();
    assert!(
        paths.contains(&format!("{run}/db").as_str())
            && paths.contains(&"/opt/scripts")
            && paths.contains(&"/etc/moi")
            && paths.contains(&"/var/lib/moi"),
        "{paths:?}"
    );
    let bundle = snapshots("moi-bundle");
    assert_eq!((bundle.len(), bundle[0]["hostname"].as_str()), (1, Some("moi-it")));
    // files in the snapshot: the scripts are there; the live SQLite file is not, its consistent copy is
    let listed = s.restic(&["ls".into(), nightly[0]["id"].as_str().unwrap().into()], &env).unwrap();
    let files = String::from_utf8_lossy(&listed.stdout);
    assert!(files.contains("/opt/scripts/resource-check.sh"), "the scripts are in the snapshot");
    assert!(
        !files.lines().any(|l| l == "/opt/services/monitoring/uptime-kuma/data/kuma.db"),
        "the live database file is left out"
    );
    assert!(
        files.contains("/var/lib/moi/staging/sqlite/opt/services/monitoring/uptime-kuma/data/kuma.db"),
        "its consistent copy is in:\n{}",
        files.lines().filter(|l| l.contains("kuma")).collect::<Vec<_>>().join("\n")
    );

    // ---- a second backup right away: local again, upload skipped by the rate limit
    let (code, out) = s.moi(&["backup"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("offsite: skipped: last upload was 0h ago (minimum 20h)"), "{out}");
    assert_eq!(snapshots("nightly").len(), 1);

    // ---- pushing the bundle again never touches the data snapshots (retention is per tag)
    assert_eq!(s.moi(&["push"]).1, "nothing changed since the last push (use --force to push anyway)\n");
    assert_eq!(s.moi(&["push", "--force"]), (0, "state bundle saved off-site\n".to_string()));
    assert_eq!((snapshots("moi-bundle").len(), snapshots("nightly").len()), (2, 1));

    // ---- the drill: the dump goes into a throwaway Postgres and the table is there
    let (code, drill) = s.moi(&["backup", "--drill"]);
    assert_eq!(code, 0, "{drill}");
    assert!(
        drill.lines().any(|l| l.contains(&format!("{name}: restored into a fresh postgres:")) && l.contains(" tables")),
        "{drill}"
    );
    assert!(
        drill.contains("files: archive read to its end; ") && drill.ends_with("the backup can be restored\n"),
        "{drill}"
    );
    let left: Vec<String> = docker
        .containers(true)
        .unwrap()
        .iter()
        .filter_map(|c| c["Names"][0].as_str().map(str::to_string))
        .filter(|n| n.contains("moi-drill"))
        .collect();
    assert_eq!(left, Vec::<String>::new(), "the throwaway container is gone");

    // ---- a new machine: only moi, restic, the repository and its password
    let fresh = Sandbox::new("it-pull");
    sudo(&["rm", "-rf", fresh.root.join("etc/moi").to_str().unwrap(), fresh.root.join("repo").to_str().unwrap()]);
    assert!(
        sudo(&["cp", "-a", s.root.join("repo").to_str().unwrap(), fresh.root.join("repo").to_str().unwrap()])
            .status
            .success()
    );
    fresh.set_env("RESTIC_PASSWORD", PASSWORD);
    let (code, out) = fresh.moi(&["pull", "--repository", "/repo"]);
    assert_eq!(code, 0, "{out}");
    let dir = out.lines().next().unwrap().split(" restored to ").nth(1).expect("where it was restored to").to_string();
    assert!(out.starts_with("state bundle of moi-it from "), "{out}");
    assert_eq!(out.lines().nth(1).unwrap(), format!("next: moi restore --bundle {dir} --dry-run"));
    // byte for byte what was pushed: the state with its whole git history, and the configuration
    sudo(&["chmod", "-R", "a+rX", fresh.root.to_str().unwrap()]);
    let restored = fresh.root.join(dir.trim_start_matches('/'));
    let (pushed_state, pulled_state) =
        (tree(&s.root.join("var/lib/moi/state")), tree(&restored.join("var/lib/moi/state")));
    assert!(pushed_state.keys().any(|k| k.starts_with(".git/objects/")), "the history is part of the bundle");
    assert_eq!(pushed_state.keys().collect::<Vec<_>>(), pulled_state.keys().collect::<Vec<_>>());
    assert!(pushed_state == pulled_state, "the restored state differs from what was pushed");
    assert!(tree(&s.root.join("etc/moi")) == tree(&restored.join("etc/moi")), "the restored configuration differs");
}
