//! Errors and the exit codes they map to.

/// Everything that stops a command.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Bad config or usage: exit 2.
    #[error("{0}")]
    Config(String),
    /// Missing tool or file the environment must provide: exit 3.
    #[error("{0}")]
    Environment(String),
    /// A check failed: exit 1.
    #[error("{0}")]
    Failed(String),
}

impl Error {
    /// The process exit code.
    pub const fn code(&self) -> u8 {
        match self {
            Self::Failed(_) => 1,
            Self::Config(_) => 2,
            Self::Environment(_) => 3,
        }
    }
}
