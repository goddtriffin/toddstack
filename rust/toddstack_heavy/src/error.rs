//! The one error type.

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Usage(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("watch: {0}")]
    Notify(#[from] notify::Error),
    #[error("system: {0}")]
    Sys(#[from] nix::Error),
    #[error("sysinfo: {0}")]
    Sysinfo(#[from] sysinfo::Error),
    #[error("{0}")]
    Probe(String),
    /// A signal arrived while waiting in the queue; the value is the signal number.
    #[error("interrupted by signal {0}")]
    Interrupted(i32),
}
