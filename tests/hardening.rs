//! Limits that keep moi from growing or hanging without end: old history folded
//! (`state.history_keep`), jobs that do not come back (docs/spec/13-performance.md, 10-daemon.md).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use cestmoi::config::{self, Paths};
use cestmoi::daemon::Daemon;
use cestmoi::host::{FixtureHost, Host, parse_rfc3339, unix_seconds};
use cestmoi::state::StateDir;
use cestmoi::state::history::History;

fn objects(host: &dyn Host, state: &StateDir) -> usize {
    let dir = state.state().join(".git/objects");
    host.read_dir(&dir)
        .unwrap()
        .iter()
        .filter(|n| n.len() == 2)
        .map(|fan| host.read_dir(&dir.join(fan)).unwrap().len())
        .sum()
}

#[test]
fn history_older_than_the_limit_becomes_one_commit_and_its_space_comes_back() {
    let host = common::minimal_rw("hardening-history");
    let state = StateDir::new("/var/lib/h");
    host.create_dir_all(&state.state(), 0o700).unwrap();
    let history = History::open(host.as_ref(), &state).unwrap();
    let day = |n: u64| parse_rfc3339("2026-01-01T12:00:00Z").unwrap() + Duration::from_secs(n * 86_400);
    // five snapshots on five days, each with a file of its own that the next one replaces
    for n in 0..5 {
        host.set_now(day(n));
        let dir = state.state().join("snapshot");
        host.create_dir_all(&dir, 0o700).unwrap();
        host.write(&dir.join("packages.txt"), format!("bash\npackage-{n}\n").as_bytes(), 0o600).unwrap();
        host.write(&dir.join(format!("only-on-day-{n}.txt")), format!("content of day {n}\n").as_bytes(), 0o600)
            .unwrap();
        if n > 0 {
            host.remove_file(&dir.join(format!("only-on-day-{}.txt", n - 1))).unwrap();
        }
        history.commit(host.as_ref(), &state).unwrap().unwrap();
    }
    let before = history.log(100).unwrap();
    let (head_before, changes_before) = history.show("HEAD").unwrap();
    let had = objects(host.as_ref(), &state);
    assert_eq!(before.len(), 5);

    // keep what is newer than day 2 at 18:00: days 0, 1, 2 are folded
    let cutoff = unix_seconds(day(2)) + 6 * 3600;
    assert_eq!(history.squash_before(host.as_ref(), &state, cutoff).unwrap(), 3);
    let after = history.log(100).unwrap();
    assert_eq!(after.len(), 3);
    assert_eq!(after[2].subject, "history up to 2026-01-03 12:00 (3 snapshots folded into one by state.history_keep)");
    assert_eq!(after[2].time, "2026-01-03T12:00:00Z", "dated like the last snapshot it stands for");
    // the newer commits say and hold what they did, under new ids
    assert_eq!(
        (after[0].subject.as_str(), after[0].time.as_str()),
        (before[0].subject.as_str(), before[0].time.as_str())
    );
    assert_eq!(after[1].subject, before[1].subject);
    let (head_after, changes_after) = history.show("HEAD").unwrap();
    assert_eq!((head_after.subject, changes_after.len()), (head_before.subject, changes_before.len()));
    assert_ne!(head_after.id, head_before.id);
    // what only the folded days had is gone from the disk; what the kept ones have is readable
    let left = objects(host.as_ref(), &state);
    assert!(left < had, "{left} objects left of {had}");
    let reopened = History::open(host.as_ref(), &state).unwrap();
    assert_eq!(reopened.log(100).unwrap().len(), 3);
    for rev in ["HEAD", "HEAD~1", "HEAD~2"] {
        reopened.show(rev).unwrap_or_else(|e| panic!("{rev}: {e}"));
    }
    // nothing more to fold: once, not every day again
    assert_eq!(reopened.squash_before(host.as_ref(), &state, cutoff).unwrap(), 0);
    // and the history goes on
    host.set_now(day(6));
    host.write(&state.state().join("snapshot/packages.txt"), b"bash\n", 0o600).unwrap();
    assert!(reopened.commit(host.as_ref(), &state).unwrap().is_some());
    assert_eq!(reopened.log(100).unwrap().len(), 4);
}

#[test]
fn the_newest_snapshot_is_never_folded_away() {
    let host = common::minimal_rw("hardening-history-all-old");
    let state = StateDir::new("/var/lib/h");
    host.create_dir_all(&state.state(), 0o700).unwrap();
    let history = History::open(host.as_ref(), &state).unwrap();
    for n in 0..3_u64 {
        host.set_now(parse_rfc3339("2026-01-01T12:00:00Z").unwrap() + Duration::from_secs(n * 86_400));
        host.create_dir_all(&state.state().join("snapshot"), 0o700).unwrap();
        host.write(&state.state().join("snapshot/n.txt"), format!("{n}\n").as_bytes(), 0o600).unwrap();
        history.commit(host.as_ref(), &state).unwrap().unwrap();
    }
    // everything is older than the limit: the two oldest fold, the newest stays a commit of its own
    assert_eq!(history.squash_before(host.as_ref(), &state, i64::MAX).unwrap(), 2);
    let log = history.log(10).unwrap();
    assert_eq!(log.len(), 2);
    assert!(log[0].subject.starts_with("changed: "), "{:?}", log[0]);
}

#[test]
fn a_job_that_does_not_come_back_is_reported_and_then_ends_the_service() {
    let fixture = Arc::new(
        FixtureHost::open(&common::fixture_dir("cestmoi"))
            .unwrap()
            .with_overlay(&common::scratch("hardening-overdue"))
            .unwrap(),
    );
    let host: Arc<dyn Host> = fixture.clone();
    let loaded = config::load(host.as_ref(), &Paths::new("/etc/moi/moi.toml".into(), None)).unwrap();
    let mut daemon = Daemon::new(host.clone(), loaded).unwrap();
    assert!(daemon.begin("daily") && daemon.begin("backup") && !daemon.begin("nope"));
    assert!(daemon.overdue().is_empty());
    fixture.advance(Duration::from_secs(29 * 60));
    assert!(daemon.overdue().is_empty(), "half an hour is the daily job's");
    fixture.advance(Duration::from_secs(2 * 60));
    let late = daemon.overdue();
    assert_eq!(late.len(), 1, "{late:?}");
    assert_eq!((late[0].job.as_str(), late[0].limit, late[0].stuck), ("daily", Duration::from_secs(1800), false));
    assert!(daemon.overdue().is_empty(), "said once");
    // twice the limit: said every time, because the loop ends the service on it
    fixture.advance(Duration::from_secs(30 * 60));
    assert!(daemon.overdue().iter().any(|l| l.job == "daily" && l.stuck));
    assert!(daemon.overdue().iter().any(|l| l.job == "daily" && l.stuck));
    // a backup takes as long as the data is big
    fixture.advance(Duration::from_secs(48 * 3600));
    assert!(!daemon.overdue().iter().any(|l| l.job == "backup"));
}
