use core::fmt;
use fleet_core::{ClientStoreErrorKind, RuntimeStoreErrorKind};

/// An error returned by the store.
#[derive(Debug)]
pub enum StoreError {
    /// The resource changed since the caller read it.
    PreconditionFailed,
    /// The operation conflicts with the stored state.
    Conflict(&'static str),
    /// An idempotency key was reused for a different request.
    IdempotencyConflict,
    /// The agent does not exist.
    AgentNotFound,
    /// The message does not exist.
    MessageNotFound,
    /// The requested revision is ahead of the agent.
    InvalidRevision,
    /// The database format is not supported.
    UnsupportedFormat,
    /// The store worker stopped.
    Closed,
    /// A database operation failed.
    Database(redb::Error),
    /// A stored record could not be encoded or decoded.
    Encoding(serde_json::Error),
    /// The worker could not be started.
    Io(std::io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PreconditionFailed => f.write_str("agent resource changed"),
            Self::Conflict(message) => f.write_str(message),
            Self::IdempotencyConflict => f.write_str("idempotency conflict"),
            Self::AgentNotFound => f.write_str("agent not found"),
            Self::MessageNotFound => f.write_str("message not found"),
            Self::InvalidRevision => f.write_str("invalid revision"),
            Self::UnsupportedFormat => f.write_str("unsupported database format"),
            Self::Closed => f.write_str("store worker stopped"),
            Self::Database(_) => f.write_str("database operation failed"),
            Self::Encoding(_) => f.write_str("record encoding failed"),
            Self::Io(_) => f.write_str("could not start store worker"),
        }
    }
}

impl core::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Encoding(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Encoding(error)
    }
}

impl From<redb::Error> for StoreError {
    fn from(error: redb::Error) -> Self {
        Self::Database(error)
    }
}

impl fleet_core::Error<RuntimeStoreErrorKind> for StoreError {
    fn kind(&self) -> RuntimeStoreErrorKind {
        match self {
            Self::Conflict(_) | Self::AgentNotFound => RuntimeStoreErrorKind::Conflict,
            _ => RuntimeStoreErrorKind::Other,
        }
    }
}

impl fleet_core::Error<ClientStoreErrorKind> for StoreError {
    fn kind(&self) -> ClientStoreErrorKind {
        match self {
            Self::Conflict(_) | Self::PreconditionFailed => ClientStoreErrorKind::Conflict,
            Self::IdempotencyConflict => ClientStoreErrorKind::IdempotencyConflict,
            Self::AgentNotFound => ClientStoreErrorKind::AgentNotFound,
            _ => ClientStoreErrorKind::Other,
        }
    }
}

impl From<redb::DatabaseError> for StoreError {
    fn from(error: redb::DatabaseError) -> Self {
        Self::Database(error.into())
    }
}

impl From<redb::TransactionError> for StoreError {
    fn from(error: redb::TransactionError) -> Self {
        Self::Database(error.into())
    }
}

impl From<redb::TableError> for StoreError {
    fn from(error: redb::TableError) -> Self {
        Self::Database(error.into())
    }
}

impl From<redb::StorageError> for StoreError {
    fn from(error: redb::StorageError) -> Self {
        Self::Database(error.into())
    }
}

impl From<redb::CommitError> for StoreError {
    fn from(error: redb::CommitError) -> Self {
        Self::Database(error.into())
    }
}
