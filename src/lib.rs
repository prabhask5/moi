//! # cestmoi — the library behind the `moi` binary
//!
//! See `CLAUDE.md` for the architecture and `docs/spec/` for behaviour. Every module below maps to a
//! spec file; modules marked with a milestone are still stubs.

#![forbid(unsafe_code)]

pub mod audit;
pub mod backup;
pub mod bundle;
pub mod checks;
pub mod cli;
pub mod config;
pub mod context;
pub mod daemon;
pub mod devbox;
pub mod docker;
pub mod error;
pub mod firewall;
pub mod heal;
pub mod host;
pub mod init;
pub mod log;
pub mod migrate;
pub mod monitors;
pub mod net;
pub mod redact;
pub mod restore;
pub mod snapshot;
pub mod ssh;
pub mod state;

/// Version string shown by `moi --version`: version, git sha, build date, target.
pub const VERSION_LONG: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("MOI_BUILD_SHA"),
    " ",
    env!("MOI_BUILD_DATE"),
    " ",
    env!("MOI_BUILD_TARGET"),
    ")"
);
