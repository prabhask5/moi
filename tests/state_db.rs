//! State dir layout and `moi.db` schema versioning (CLAUDE.md § Files on a server, docs/spec/02-snapshot.md § moi.db).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::time::Duration;

use cestmoi::host::{FileKind, Host};
use cestmoi::state::StateDir;
use cestmoi::state::db::{Db, MIGRATIONS, Migration};

#[test]
fn layout_matches_the_documented_paths() {
    let s = StateDir::new("/var/lib/moi");
    assert_eq!(s.root(), Path::new("/var/lib/moi"));
    assert_eq!(s.state(), Path::new("/var/lib/moi/state"));
    assert_eq!(s.snapshot(), Path::new("/var/lib/moi/state/snapshot"));
    assert_eq!(s.volatile(), Path::new("/var/lib/moi/state/volatile"));
    assert_eq!(s.notes(), Path::new("/var/lib/moi/state/NOTES.md"));
    assert_eq!(s.context(), Path::new("/var/lib/moi/state/CONTEXT.md"));
    assert_eq!(s.audit(), Path::new("/var/lib/moi/state/audit.md"));
    assert_eq!(s.db(), Path::new("/var/lib/moi/moi.db"));
    assert_eq!(s.archive(), Path::new("/var/lib/moi/archive"));
}

#[test]
fn ensure_creates_private_directories_and_is_idempotent() {
    let host = common::minimal_rw("state-ensure");
    let s = StateDir::new("/var/lib/moi");
    s.ensure(host.as_ref()).unwrap();
    s.ensure(host.as_ref()).unwrap();
    for dir in [s.root().to_path_buf(), s.state(), s.archive()] {
        let md = host.metadata(&dir).unwrap();
        assert_eq!((md.kind, md.mode), (FileKind::Dir, 0o700), "{}", dir.display());
    }
}

#[test]
fn ensure_tightens_a_state_dir_that_is_too_open() {
    let host = common::minimal_rw("state-tighten");
    host.create_dir_all(Path::new("/var/lib/moi"), 0o755).unwrap();
    StateDir::new("/var/lib/moi").ensure(host.as_ref()).unwrap();
    assert_eq!(host.metadata(Path::new("/var/lib/moi")).unwrap().mode, 0o700);
}

fn tables(db: &Db) -> Vec<String> {
    let mut stmt = db.conn().prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
}

#[test]
fn a_new_database_gets_the_current_schema() {
    let host = common::minimal_rw("db-new");
    let s = StateDir::new("/var/lib/moi");
    s.ensure(host.as_ref()).unwrap();
    let db = s.open_db(host.as_ref()).unwrap();
    assert_eq!(db.schema_version().unwrap(), MIGRATIONS.last().unwrap().version);
    assert_eq!(
        tables(&db),
        [
            "check_results",
            "check_values",
            "heal_cooldowns",
            "monitor_bindings",
            "schedule_runs",
            "schema_history",
            "sustained"
        ]
    );
    // the file is private
    assert_eq!(host.metadata(&s.db()).unwrap().mode, 0o600);
    // history records when each migration ran, on the host's clock
    let (name, at): (String, i64) = db
        .conn()
        .query_row("SELECT name, applied_at FROM schema_history WHERE version = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(name, MIGRATIONS[0].name);
    assert_eq!(at, 1_772_366_400, "2026-03-01T12:00:00Z from the fixture clock");
}

#[test]
fn shipped_migrations_are_numbered_from_one_without_gaps() {
    for (i, m) in MIGRATIONS.iter().enumerate() {
        assert_eq!(m.version as usize, i + 1, "{}", m.name);
    }
}

const V1: Migration =
    Migration { version: 1, name: "one", sql: "CREATE TABLE a (x INTEGER); INSERT INTO a VALUES (1);" };
const V2: Migration =
    Migration { version: 2, name: "two", sql: "ALTER TABLE a ADD COLUMN y TEXT; UPDATE a SET y = 'kept';" };
const V3_BROKEN: Migration =
    Migration { version: 3, name: "broken", sql: "CREATE TABLE b (x INTEGER); INSERT INTO no_such_table VALUES (1);" };

#[test]
fn migrations_run_forward_once_and_keep_data() {
    let host = common::minimal_rw("db-forward");
    let path = Path::new("/moi.db");
    {
        let db = Db::open_with(host.as_ref(), path, &[V1]).unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);
    }
    host.advance(Duration::from_secs(60));
    {
        let db = Db::open_with(host.as_ref(), path, &[V1, V2]).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
        let (x, y): (i64, String) =
            db.conn().query_row("SELECT x, y FROM a", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((x, y.as_str()), (1, "kept"));
    }
    // reopening applies nothing again (V1's INSERT would add a second row)
    let db = Db::open_with(host.as_ref(), path, &[V1, V2]).unwrap();
    let rows: i64 = db.conn().query_row("SELECT count(*) FROM a", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1);
    let applied: Vec<(u32, i64)> = {
        let mut st = db.conn().prepare("SELECT version, applied_at FROM schema_history ORDER BY version").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(applied, [(1, 1_772_366_400), (2, 1_772_366_460)]);
}

#[test]
fn a_failing_migration_rolls_back_completely_and_reports_which_one() {
    let host = common::minimal_rw("db-rollback");
    let path = Path::new("/moi.db");
    let err = Db::open_with(host.as_ref(), path, &[V1, V2, V3_BROKEN]).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    let msg = err.to_string();
    assert!(msg.contains("migration 3 (broken)"), "{msg}");
    // versions 1 and 2 stay applied, nothing of 3 remains
    let db = Db::open_with(host.as_ref(), path, &[V1, V2]).unwrap();
    assert_eq!(db.schema_version().unwrap(), 2);
    assert_eq!(tables(&db), ["a", "schema_history"]);
}

#[test]
fn a_database_from_a_newer_moi_is_refused_untouched() {
    let host = common::minimal_rw("db-newer");
    let path = Path::new("/moi.db");
    drop(Db::open_with(host.as_ref(), path, &[V1, V2]).unwrap());
    let err = Db::open_with(host.as_ref(), path, &[V1]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "state database: /moi.db has schema version 2 but this moi only knows version 1: upgrade moi (never downgrade over existing state)"
    );
    assert_eq!(Db::open_with(host.as_ref(), path, &[V1, V2]).unwrap().schema_version().unwrap(), 2);
}

#[test]
fn a_misnumbered_migration_list_is_rejected() {
    let host = common::minimal_rw("db-misnumbered");
    let err = Db::open_with(host.as_ref(), Path::new("/moi.db"), &[V2]).unwrap_err();
    assert!(err.to_string().contains("migrations must be numbered 1, 2, 3"), "{err}");
}
