//! Library error type and process exit codes.
//!
//! Exit codes (stable, documented in README):
//! - 0 success
//! - 1 a check/verify/audit found problems (the command itself worked)
//! - 2 usage, config or runtime error
//! - 3 not implemented yet

use thiserror::Error;

use crate::config::Diagnostic;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{what} is not implemented yet (milestone {milestone}, see docs/milestones.md)")]
    NotImplemented { what: &'static str, milestone: &'static str },

    #[error("problems found")]
    ProblemsFound,

    /// The configuration or secrets file is invalid; every problem found, in file order.
    #[error("{}", join_diagnostics(.0))]
    ConfigInvalid(Vec<Diagnostic>),

    #[error("state database: {0}")]
    Db(String),

    /// An OS operation failed; `context` says what moi was doing.
    #[error("{context}: {source}")]
    Host {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::ProblemsFound => 1,
            Error::NotImplemented { .. } => 3,
            Error::ConfigInvalid(_) | Error::Db(_) | Error::Host { .. } | Error::Io(_) => 2,
        }
    }

    /// Stable machine-readable name, used in `--json` error output.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::NotImplemented { .. } => "not_implemented",
            Error::ProblemsFound => "problems_found",
            Error::ConfigInvalid(_) => "config",
            Error::Db(_) => "state",
            Error::Host { .. } | Error::Io(_) => "io",
        }
    }

    /// Wrap an I/O error with what was being attempted.
    pub fn host(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Host { context: context.into(), source }
    }
}

fn join_diagnostics(diags: &[Diagnostic]) -> String {
    diags.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
}
