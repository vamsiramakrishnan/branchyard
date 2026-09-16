//! Durable control-plane admission. No harness or host command execution.
pub mod config;
pub mod graph;
pub mod http;
pub mod store;
pub use store::Store;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid command")]
    Invalid,
    #[error("authentication required")]
    Unauthorized,
    #[error("outside delegated authority")]
    Forbidden,
    #[error("identity or revision conflict")]
    Conflict,
    #[error("resource envelope exhausted")]
    Capacity,
    #[error("required profile or capability unavailable")]
    Unsupported,
    #[error("not found")]
    NotFound,
    #[error("invalid server configuration")]
    Config,
    #[error("database operation failed; submission may require reconciliation")]
    Database(#[from] sqlx::Error),
    #[error("stored state is invalid")]
    Corrupt,
}
