//! `moi.db`: SQLite with a versioned schema and forward-only migrations.
//!
//! The schema version is SQLite's `user_version`. Opening applies every migration newer than it, each
//! in its own transaction; a database written by a newer moi is refused untouched, so a downgrade can
//! never corrupt state. Shipped migrations are never edited — a schema change is a new migration.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::error::Error;
use crate::host::{self, Host};

/// One schema step. `sql` may contain several statements.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Every schema step moi has shipped, oldest first. Append only.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "checks, monitors, heal and schedule state",
        sql: "
        -- last N results per check (05)
        CREATE TABLE check_results (
            id          INTEGER PRIMARY KEY,
            check_name  TEXT    NOT NULL,
            at          INTEGER NOT NULL,                 -- unix seconds
            status      TEXT    NOT NULL CHECK (status IN ('up', 'down')),
            message     TEXT    NOT NULL,
            duration_ms INTEGER NOT NULL DEFAULT 0,
            pushed      INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX check_results_by_check ON check_results (check_name, at);

        -- conditions that must hold for N consecutive runs before they are reported (05)
        CREATE TABLE sustained (
            check_name TEXT    NOT NULL,
            condition  TEXT    NOT NULL,
            runs       INTEGER NOT NULL,
            since      INTEGER NOT NULL,
            PRIMARY KEY (check_name, condition)
        ) WITHOUT ROWID;

        -- values of the previous run, for deltas: CPU counters, restart counts (05)
        CREATE TABLE check_values (
            check_name TEXT    NOT NULL,
            key        TEXT    NOT NULL,
            value      TEXT    NOT NULL,
            at         INTEGER NOT NULL,
            PRIMARY KEY (check_name, key)
        ) WITHOUT ROWID;

        -- which Kuma monitor belongs to which check/container/job, and its push token (06)
        CREATE TABLE monitor_bindings (
            owner          TEXT    PRIMARY KEY,           -- e.g. check:resources, container:jellyfin
            monitor_name   TEXT    NOT NULL,
            kuma_id        INTEGER,
            push_token     TEXT,
            created_by_moi INTEGER NOT NULL DEFAULT 0,    -- only these may ever be pruned
            updated_at     INTEGER NOT NULL
        ) WITHOUT ROWID;

        -- when each heal rule last acted (10)
        CREATE TABLE heal_cooldowns (
            rule     TEXT    PRIMARY KEY,
            last_run INTEGER NOT NULL
        ) WITHOUT ROWID;

        -- last run of each scheduled job, so missed calendar jobs are caught up after boot (10)
        CREATE TABLE schedule_runs (
            job         TEXT    PRIMARY KEY,
            last_run    INTEGER NOT NULL,
            last_status TEXT
        ) WITHOUT ROWID;
    ",
    },
    Migration {
        version: 2,
        name: "snapshot file table and capture cache",
        sql: "
        -- every file in state/snapshot and state/volatile: which collector owns it, and for copies of
        -- host files the (size, mtime, inode) -> sha256 cache that lets unchanged files be skipped (02)
        CREATE TABLE snapshot_files (
            path      TEXT    PRIMARY KEY,                -- relative to state/, e.g. snapshot/files/etc/hosts
            collector TEXT    NOT NULL,
            kind      TEXT    NOT NULL CHECK (kind IN ('gen', 'file', 'link', 'skipped')),
            source    TEXT    NOT NULL DEFAULT '',        -- host path of a copied file
            size      INTEGER NOT NULL DEFAULT 0,
            mtime_ns  INTEGER NOT NULL DEFAULT 0,
            inode     INTEGER NOT NULL DEFAULT 0,
            mode      INTEGER NOT NULL DEFAULT 0,
            uid       INTEGER NOT NULL DEFAULT 0,
            gid       INTEGER NOT NULL DEFAULT 0,
            sha256    TEXT    NOT NULL DEFAULT ''         -- '-> target' for symlinks, '-' if not hashed
        ) WITHOUT ROWID;
    ",
    },
    Migration {
        version: 3,
        name: "context shares and settings",
        sql: "
        -- copies of CONTEXT.md uploaded for sharing, so expired ones can be deleted and listed (03)
        CREATE TABLE context_shares (
            key        TEXT    PRIMARY KEY,               -- object key in the share bucket: ctx/<random>.md
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL
        ) WITHOUT ROWID;

        -- small facts moi remembers about what it already did
        CREATE TABLE settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        ) WITHOUT ROWID;
    ",
    },
    Migration {
        version: 4,
        name: "backup runs",
        sql: "
        -- one row per backup run: what `moi status`, the audit and the context file say about backups
        CREATE TABLE backup_runs (
            id         INTEGER PRIMARY KEY,
            at         INTEGER NOT NULL,
            status     TEXT    NOT NULL,              -- ok | failed
            bytes      INTEGER NOT NULL DEFAULT 0,    -- size of the local run directory
            duration_s INTEGER NOT NULL DEFAULT 0,
            message    TEXT    NOT NULL DEFAULT ''    -- the last error, or the directory
        );
    ",
    },
];

/// An open `moi.db` at the current schema.
#[derive(Debug)]
pub struct Db {
    conn: Connection,
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Db(e.to_string())
}

impl Db {
    /// Open (creating or migrating) the database at a host path with the shipped [`MIGRATIONS`].
    pub fn open(host: &dyn Host, path: &Path) -> Result<Self, Error> {
        Self::open_with(host, path, MIGRATIONS)
    }

    /// Like [`Db::open`] with an explicit migration list (tests exercise the migrator with their own).
    pub fn open_with(host: &dyn Host, path: &Path, migrations: &[Migration]) -> Result<Self, Error> {
        if let Some(bad) = migrations.iter().enumerate().find(|(i, m)| m.version as usize != i + 1) {
            return Err(Error::Db(format!(
                "migrations must be numbered 1, 2, 3, … without gaps (found version {} at position {})",
                bad.1.version,
                bad.0 + 1
            )));
        }
        let real = host
            .real_path(path)
            .map_err(|e| Error::host(format!("cannot open state database {}", path.display()), e))?;
        let created = !host.exists(path);
        let mut conn =
            Connection::open(&real).map_err(|e| Error::Db(format!("cannot open {}: {e}", path.display())))?;
        if created {
            // Before anything is stored: check results and push tokens are nobody else's business.
            host.set_mode(path, 0o600)
                .map_err(|e| Error::host(format!("cannot protect state database {}", path.display()), e))?;
        }
        conn.busy_timeout(Duration::from_secs(5)).map_err(db_err)?;
        conn.pragma_update(None, "foreign_keys", true).map_err(db_err)?;

        let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(db_err)?;
        let known = migrations.last().map_or(0, |m| m.version);
        if current > known {
            return Err(Error::Db(format!(
                "{} has schema version {current} but this moi only knows version {known}: upgrade moi (never downgrade over existing state)",
                path.display()
            )));
        }
        let now = host::unix_seconds(host.now());
        for m in migrations.iter().filter(|m| m.version > current) {
            let failed = |e: rusqlite::Error| {
                Error::Db(format!("migration {} ({}) failed, nothing of it was applied: {e}", m.version, m.name))
            };
            let tx = conn.transaction().map_err(failed)?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_history (
                    version    INTEGER PRIMARY KEY,
                    name       TEXT    NOT NULL,
                    applied_at INTEGER NOT NULL
                ) WITHOUT ROWID;",
            )
            .map_err(failed)?;
            tx.execute_batch(m.sql).map_err(failed)?;
            tx.execute(
                "INSERT INTO schema_history (version, name, applied_at) VALUES (?1, ?2, ?3)",
                (m.version, m.name, now),
            )
            .map_err(failed)?;
            tx.pragma_update(None, "user_version", m.version).map_err(failed)?;
            tx.commit().map_err(failed)?;
        }
        Ok(Self { conn })
    }

    /// The schema version stored in the file.
    pub fn schema_version(&self) -> Result<u32, Error> {
        self.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(db_err)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// A small fact moi remembers (the `settings` table).
    pub fn setting(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).ok()
    }

    pub fn set_setting(&self, key: &str, value: &str) {
        let _ = self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        );
    }

    pub fn unset_setting(&self, key: &str) {
        let _ = self.conn.execute("DELETE FROM settings WHERE key = ?1", [key]);
    }
}
