//! History (docs/spec/02-snapshot.md § History): one commit per snapshot that changed something, with a
//! subject and body that say what changed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cestmoi::config::{self, Config, Paths};
use cestmoi::docker::Docker;
use cestmoi::host::{FixtureHost, Host};
use cestmoi::snapshot;
use cestmoi::state::StateDir;
use cestmoi::state::diff::unified;
use cestmoi::state::history::{self, Entry, History, Status};

struct Server {
    fixture: Arc<FixtureHost>,
    host: Arc<dyn Host>,
    config: Arc<Config>,
    state: StateDir,
    overlay: PathBuf,
}

fn server(test: &str) -> Server {
    let overlay = common::scratch(test);
    let fixture = Arc::new(FixtureHost::open(&common::fixture_dir("cestmoi")).unwrap().with_overlay(&overlay).unwrap());
    let loaded = config::load(fixture.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let state = StateDir::new(loaded.config.state.dir.clone());
    Server { host: fixture.clone(), fixture, config: Arc::new(loaded.config), state, overlay }
}

impl Server {
    /// Snapshot and record, like `moi snapshot`. The clock moves a minute each time.
    fn snapshot(&self) -> Option<Entry> {
        self.state.ensure(self.host.as_ref()).unwrap();
        let db = self.state.open_db(self.host.as_ref()).unwrap();
        snapshot::run(&self.host, &self.config, &self.state, &db).unwrap();
        let commit = history::record(self.host.as_ref(), &self.state).unwrap();
        self.fixture.advance(Duration::from_secs(60));
        commit
    }

    fn history(&self) -> History {
        History::open(self.host.as_ref(), &self.state).unwrap()
    }

    fn write(&self, path: &str, text: &str) {
        self.host.write(Path::new(path), text.as_bytes(), 0o644).unwrap();
    }

    fn change_container(&self, name: &str, change: impl FnOnce(&mut serde_json::Value)) {
        let mut c = Docker::new(self.host.clone()).container(name).unwrap();
        change(&mut c);
        self.fixture.set_docker(&format!("/containers/{}/json", c["Id"].as_str().unwrap()), &c.to_string());
    }
}

#[test]
fn two_snapshots_without_a_change_make_one_commit() {
    let s = server("hist-one-commit");
    let first = s.snapshot().expect("the first snapshot is always a commit");
    assert_eq!(first.subject, "initial snapshot of cestmoi");
    assert_eq!(first.time, "2026-11-26T17:00:00Z", "commit time is the host's clock");
    assert_eq!(first.id.len(), 40);
    assert_eq!(s.snapshot(), None);
    assert_eq!(s.snapshot(), None);
    assert_eq!(s.history().log(10).unwrap(), [first]);
}

#[test]
fn only_stable_state_is_committed() {
    let s = server("hist-ignore");
    s.snapshot();
    let state = s.state.state();
    assert_eq!(s.host.read_to_string(&state.join(".gitignore")).unwrap(), "volatile/\nCONTEXT.md\naudit.md\n");
    let (_, changes) = s.history().show("HEAD").unwrap();
    assert!(changes.iter().all(|c| c.status == Status::Added));
    assert!(
        changes.iter().all(|c| c.path.starts_with("snapshot/") || c.path == ".gitignore"),
        "volatile/ must stay out"
    );
    assert!(changes.iter().any(|c| c.path == "snapshot/files/etc/shadow"));
    // a symlink is committed as a symlink
    let link = changes.iter().find(|c| c.path == "snapshot/files/etc/localtime").unwrap();
    assert_eq!(link.new.as_deref(), Some(b"/usr/share/zoneinfo/America/New_York".as_slice()));
}

#[test]
fn editing_a_file_in_etc_is_one_commit_naming_the_file() {
    let s = server("hist-etc");
    s.snapshot();
    s.write("/etc/hosts", "127.0.0.1 localhost\n192.168.1.10 cestmoi\n");
    let c = s.snapshot().unwrap();
    assert_eq!(c.subject, "changed: /etc/hosts");
    assert_eq!(c.body, "M\t/etc/hosts");
    assert_eq!(c.time, "2026-11-26T17:01:00Z");
    assert_eq!(s.snapshot(), None);
}

#[test]
fn adding_a_cron_line_names_the_area_and_the_file() {
    let s = server("hist-cron");
    s.snapshot();
    let path = "/var/spool/cron/crontabs/prabhas";
    let old = s.host.read_to_string(Path::new(path)).unwrap();
    s.host
        .write(
            Path::new(path),
            format!("{old}30 2 * * * /home/prabhas/bin/weekly-note.sh --nightly\n").as_bytes(),
            0o600,
        )
        .unwrap();
    let c = s.snapshot().unwrap();
    assert_eq!(c.subject, "changed: cron, /var/spool/cron/crontabs/prabhas");
    assert_eq!(c.body, "M\tsnapshot/cron/user-prabhas.crontab\nM\t/var/spool/cron/crontabs/prabhas");
}

#[test]
fn a_containers_env_change_is_described_in_the_body() {
    let s = server("hist-container-env");
    s.snapshot();
    s.change_container("sonarr", |c| {
        c["Config"]["Env"].as_array_mut().unwrap().push(serde_json::json!("SONARR__LOG__LEVEL=debug"));
        c["Mounts"].as_array_mut().unwrap().pop();
    });
    let c = s.snapshot().unwrap();
    assert_eq!(c.subject, "changed: docker");
    assert_eq!(c.body, "~ container sonarr: env, mounts\nM\tsnapshot/docker/containers.json");
}

#[test]
fn containers_added_and_removed_are_listed_with_their_image() {
    let s = server("hist-container-add-remove");
    s.snapshot();
    let docker = Docker::new(s.host.clone());
    let mut list = docker.containers(true).unwrap();
    list.retain(|c| c["Names"][0] != "/watchtower");
    let mut whoami = docker.container("dozzle").unwrap();
    let id = "7".repeat(64);
    whoami["Id"] = serde_json::json!(id);
    whoami["Name"] = serde_json::json!("/whoami");
    whoami["Config"]["Image"] = serde_json::json!("traefik/whoami:v1.11");
    list.push(serde_json::json!({ "Id": id, "Names": ["/whoami"] }));
    s.fixture.set_docker("/containers/json", &serde_json::Value::Array(list).to_string());
    s.fixture.set_docker(&format!("/containers/{id}/json"), &whoami.to_string());
    let c = s.snapshot().unwrap();
    assert_eq!(c.subject, "changed: docker");
    assert_eq!(
        c.body,
        "- container watchtower (containrrr/watchtower:1.7.1)\n+ container whoami (traefik/whoami:v1.11)\n\
         M\tsnapshot/docker/compose-projects.json\nM\tsnapshot/docker/containers.json"
    );
}

#[test]
fn a_new_swarm_service_is_one_commit_describing_it() {
    let s = server("hist-service");
    s.snapshot();
    let mut services = Docker::new(s.host.clone()).services().unwrap();
    let mut new = services.iter().find(|x| x["Spec"]["Name"] == "dokploy-postgres").unwrap().clone();
    new["Spec"]["Name"] = serde_json::json!("blog-z1y2");
    services.push(new);
    s.fixture.set_docker("/services", &serde_json::Value::Array(services).to_string());
    let c = s.snapshot().unwrap();
    assert_eq!(c.subject, "changed: docker");
    assert_eq!(c.body, "+ service blog-z1y2 (postgres:16)\nM\tsnapshot/docker/services.json");
    assert_eq!(s.snapshot(), None);
}

#[test]
fn a_subject_lists_at_most_four_files() {
    let s = server("hist-many-files");
    s.snapshot();
    for n in 1..=6 {
        s.write(&format!("/etc/motd-{n}"), "hi\n");
    }
    s.host.remove_file(Path::new("/etc/motd-1")).ok();
    s.write("/etc/motd-1", "hi\n");
    let c = s.snapshot().unwrap();
    assert_eq!(c.subject, "changed: /etc/motd-1, /etc/motd-2, /etc/motd-3, /etc/motd-4 +2 files");
    assert_eq!(c.body.lines().count(), 6);
    assert!(c.body.lines().all(|l| l.starts_with("A\t/etc/motd-")));

    for n in 1..=6 {
        s.host.remove_file(Path::new(&format!("/etc/motd-{n}"))).unwrap();
    }
    let c = s.snapshot().unwrap();
    assert!(c.body.lines().all(|l| l.starts_with("D\t/etc/motd-")), "{}", c.body);
}

#[test]
fn the_owners_notes_are_versioned_too() {
    let s = server("hist-notes");
    s.snapshot();
    s.host.write(&s.state.notes(), b"# Notes\n\nThe USB disk is on the left port.\n", 0o600).unwrap();
    let c = s.snapshot().unwrap();
    assert_eq!((c.subject.as_str(), c.body.as_str()), ("changed: NOTES.md", "A\tNOTES.md"));
}

#[test]
fn log_show_and_changelog() {
    let s = server("hist-log");
    s.snapshot();
    s.write("/etc/hosts", "127.0.0.1 localhost\n");
    s.snapshot();
    s.write("/etc/motd", "welcome\n");
    s.snapshot();
    let h = s.history();
    let log = h.log(10).unwrap();
    let subjects: Vec<&str> = log.iter().map(|e| e.subject.as_str()).collect();
    assert_eq!(subjects, ["changed: /etc/motd", "changed: /etc/hosts", "initial snapshot of cestmoi"]);
    assert_eq!(h.log(2).unwrap().len(), 2);

    let (commit, changes) = h.show("HEAD~1").unwrap();
    assert_eq!(commit.subject, "changed: /etc/hosts");
    let paths: Vec<(&str, Status)> = changes.iter().map(|c| (c.path.as_str(), c.status)).collect();
    assert_eq!(
        paths,
        [("snapshot/files.manifest.tsv", Status::Modified), ("snapshot/files/etc/hosts", Status::Modified)]
    );
    assert_eq!(changes[1].old.as_deref(), Some(b"127.0.0.1 localhost\n127.0.1.1 cestmoi\n".as_slice()));
    assert_eq!(changes[1].new.as_deref(), Some(b"127.0.0.1 localhost\n".as_slice()));
    assert_eq!(h.show(&log[1].id[..10]).unwrap().0, commit, "an id prefix is a revision too");
    let err = h.show("HEAD~9").unwrap_err();
    assert_eq!(err.to_string(), "history: no such revision: HEAD~9 (see `moi log`)");

    assert_eq!(
        s.host.read_to_string(&s.state.volatile().join("changelog.txt")).unwrap(),
        "### 2026-11-26 17:02 — changed: /etc/motd\nA\t/etc/motd\n\
         ### 2026-11-26 17:01 — changed: /etc/hosts\nM\t/etc/hosts\n\
         ### 2026-11-26 17:00 — initial snapshot of cestmoi\n"
    );
    // the changelog survives the next snapshot although no collector produces it
    s.snapshot();
    assert!(s.host.exists(&s.state.volatile().join("changelog.txt")));
}

#[test]
fn the_repository_is_a_real_git_repository() {
    let s = server("hist-git-interop");
    s.snapshot();
    s.write("/etc/hosts", "127.0.0.1 localhost\n");
    s.snapshot();
    let repo = s.overlay.join("var/lib/moi/state");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            // nothing of the machine's own git setup may influence (or be touched by) the check
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "safe.directory")
            .env("GIT_CONFIG_VALUE_0", "*")
            .output()
    };
    let Ok(log) = git(&["log", "--format=%an <%ae> %s"]) else {
        eprintln!("git is not installed: skipping the interoperability check");
        return;
    };
    assert!(log.status.success(), "{}", String::from_utf8_lossy(&log.stderr));
    assert_eq!(
        String::from_utf8_lossy(&log.stdout),
        "moi <moi@cestmoi> changed: /etc/hosts\nmoi <moi@cestmoi> initial snapshot of cestmoi\n"
    );
    let fsck = git(&["fsck", "--strict", "--no-dangling"]).unwrap();
    assert!(fsck.status.success(), "{}", String::from_utf8_lossy(&fsck.stderr));
    let files = git(&["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
    let files = String::from_utf8_lossy(&files.stdout).into_owned();
    assert!(files.contains("snapshot/files/etc/hosts\n") && !files.contains("volatile/"), "{files}");
}

// ------------------------------------------------------------------ unified diff

#[test]
fn unified_diff_of_a_small_change() {
    let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
    let new = "a\nb\nc\nD\ne\nf\ng\nh\ni\n";
    assert_eq!(unified(old, new, 1), "@@ -3,3 +3,3 @@\n c\n-d\n+D\n e\n@@ -8,1 +8,2 @@\n h\n+i\n");
    assert_eq!(unified(old, new, 3), "@@ -1,8 +1,9 @@\n a\n b\n c\n-d\n+D\n e\n f\n g\n h\n+i\n");
    assert_eq!(unified(old, old, 3), "");
}

#[test]
fn unified_diff_of_added_and_removed_files() {
    assert_eq!(unified("", "one\ntwo\n", 3), "@@ -0,0 +1,2 @@\n+one\n+two\n");
    assert_eq!(unified("one\ntwo\n", "", 3), "@@ -1,2 +0,0 @@\n-one\n-two\n");
    assert_eq!(unified("x\n", "y\n", 3), "@@ -1,1 +1,1 @@\n-x\n+y\n");
}

#[test]
fn unified_diff_applies_cleanly() {
    // property: replaying the hunks over the old text gives the new text
    let old: Vec<String> = (0..200).map(|i| format!("line {i}")).collect();
    let mut new = old.clone();
    new.remove(150);
    new[40] = "changed".into();
    new.insert(10, "inserted".into());
    new.push("tail".into());
    let (old_text, new_text) = (old.join("\n") + "\n", new.join("\n") + "\n");
    let diff = unified(&old_text, &new_text, 2);
    let mut result: Vec<String> = Vec::new();
    let mut cursor = 0;
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("@@ -") {
            let start: usize = header.split([',', ' ']).next().unwrap().parse().unwrap();
            let count: usize = header.split([',', ' ']).nth(1).unwrap().parse().unwrap();
            let start = if count == 0 { start } else { start - 1 };
            result.extend(old[cursor..start].iter().cloned());
            cursor = start;
        } else if let Some(kept) = line.strip_prefix(' ') {
            assert_eq!(old[cursor], kept);
            result.push(kept.to_string());
            cursor += 1;
        } else if line.starts_with('-') {
            cursor += 1;
        } else if let Some(added) = line.strip_prefix('+') {
            result.push(added.to_string());
        }
    }
    result.extend(old[cursor..].iter().cloned());
    assert_eq!(result, new);
}
