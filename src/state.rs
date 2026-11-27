//! State dir layout and `moi.db`. Git history, change summaries and NOTES.md stubs arrive in M2/M3.
//!
//! Spec: docs/spec/02-snapshot.md § State layout, § moi.db; CLAUDE.md § Files on a server.

pub mod db;
pub mod diff;
pub mod history;
pub mod notes;

use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::host::Host;

/// The state root (`state.dir`, default `/var/lib/moi`): private to root because it holds copies of
/// `/etc/shadow`, `.env` files and SSH keys (docs/spec/14-security.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The git repository.
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// Stable captured state (versioned).
    pub fn snapshot(&self) -> PathBuf {
        self.state().join("snapshot")
    }

    /// Volatile captured state (git-ignored).
    pub fn volatile(&self) -> PathBuf {
        self.state().join("volatile")
    }

    pub fn notes(&self) -> PathBuf {
        self.state().join("NOTES.md")
    }

    pub fn context(&self) -> PathBuf {
        self.state().join("CONTEXT.md")
    }

    pub fn audit(&self) -> PathBuf {
        self.state().join("audit.md")
    }

    pub fn db(&self) -> PathBuf {
        self.root.join("moi.db")
    }

    /// Everything moi replaced or removed, for rollback.
    pub fn archive(&self) -> PathBuf {
        self.root.join("archive")
    }

    /// Create the root, `state/` and `archive/` (0700); tighten the root if it already exists more open.
    pub fn ensure(&self, host: &dyn Host) -> Result<(), Error> {
        for dir in [self.root.clone(), self.state(), self.archive()] {
            let ctx = |e| Error::host(format!("cannot prepare state directory {}", dir.display()), e);
            host.create_dir_all(&dir, 0o700).map_err(ctx)?;
            if host.metadata(&dir).map_err(ctx)?.mode != 0o700 {
                host.set_mode(&dir, 0o700).map_err(ctx)?;
            }
        }
        Ok(())
    }

    /// Open `moi.db`, creating or migrating it to the current schema.
    pub fn open_db(&self, host: &dyn Host) -> Result<db::Db, Error> {
        db::Db::open(host, &self.db())
    }
}
