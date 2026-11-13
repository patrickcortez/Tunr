use thiserror::Error;

/// Unified error type for Tunr CLI, library, audio pipeline, IPC, and daemon.
#[derive(Debug, Error)]
pub enum TunrError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid argument: {0}")]
    InvalidArg(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("ambiguous match: {0}")]
    Ambiguous(String),

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("audio error: {0}")]
    Audio(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("daemon error: {0}")]
    Daemon(String),

    #[error("{0}")]
    NotInitialized(String),

    #[error("download error: {0}")]
    Download(String),

    #[error("search error: {0}")]
    Search(String),

    #[error("async task error: {0}")]
    Async(String),
}

pub type Result<T> = std::result::Result<T, TunrError>;
