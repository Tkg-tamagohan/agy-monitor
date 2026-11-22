pub mod analyzer;
pub mod fingerprint;
pub mod models;
pub mod monitor;
pub mod paths;
pub mod proto;
pub mod sessions;
pub mod sqlite;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("SQLite database is busy or locked after retries: {path}")]
    DbBusy { path: String },

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Home directory could not be determined")]
    NoHomeDir,

    #[error("Invalid conversation ID: {0}")]
    InvalidConversationId(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
