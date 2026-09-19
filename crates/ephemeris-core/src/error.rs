use thiserror::Error;

/// Ephemeris application error type.
#[derive(Error, Debug)]
pub enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Not found: {0}")]
    NotFound(String),

    /// Backend-level error from an external service (e.g. WebDAV, HTTP).
    ///
    /// Wraps the stringified error so that callers do not need to depend on the
    /// concrete backend crate's error types.
    #[error("Backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, AppError>;
