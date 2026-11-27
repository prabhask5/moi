//! Library error type and process exit codes.
//!
//! Exit codes (stable, documented in README):
//! - 0 success
//! - 1 a check/verify/audit found problems (the command itself worked)
//! - 2 usage, config or runtime error
//! - 3 not implemented yet

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{what} is not implemented yet (milestone {milestone}, see docs/milestones.md)")]
    NotImplemented { what: &'static str, milestone: &'static str },

    #[error("problems found")]
    ProblemsFound,

    #[error("config: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::ProblemsFound => 1,
            Error::NotImplemented { .. } => 3,
            Error::Config(_) | Error::Io(_) => 2,
        }
    }
}
