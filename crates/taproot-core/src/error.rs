use thiserror::Error;

/// Core error type for Taproot operations.
#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(String),

    #[error("introspection error: {0}")]
    Introspection(String),

    #[error("schema error: {0}")]
    Schema(String),
}

impl From<sqlx::Error> for Error {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err.to_string())
    }
}

/// Convenience alias for Taproot results.
pub type Result<T> = std::result::Result<T, Error>;
