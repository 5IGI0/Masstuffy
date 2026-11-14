//! Error types and the mapping from redb errors.

use masstuffy_index::Error;

/// Error of [`RedbIndex::open`](crate::RedbIndex::open).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The file is open elsewhere (this process included).
    #[error("index file already open")]
    AlreadyOpen,
    /// The file was written with another [`SCHEMA_VERSION`](crate::SCHEMA_VERSION).
    #[error("index schema version {found}, expected {expected}")]
    SchemaMismatch {
        /// Version found in the file.
        found: u64,
        /// Version this build expects.
        expected: u64,
    },
    /// Not a redb file, not a Masstuffy index, or failing redb's checks.
    #[error("corrupt index file: {0}")]
    Corrupt(String),
    /// A redb file format this redb cannot open.
    #[error("unsupported index file format: {0}")]
    Unsupported(String),
    /// I/O error while opening.
    #[error("index file I/O error")]
    Io(#[source] std::io::Error),
    /// Any other backend error.
    #[error("index backend error")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl OpenError {
    /// `true` for `SchemaMismatch`, `Corrupt` and `Unsupported`: the engine deletes the file and
    /// rebuilds the index from the CDX files.
    #[must_use]
    pub fn needs_rebuild(&self) -> bool {
        matches!(
            self,
            OpenError::SchemaMismatch { .. } | OpenError::Corrupt(_) | OpenError::Unsupported(_)
        )
    }
}

/// Insert of a string longer than `u32::MAX` bytes (cannot be encoded).
#[derive(Debug, thiserror::Error)]
#[error("field too long to store: {0} bytes")]
pub(crate) struct FieldTooLong(pub usize);

/// redb error -> index error. `Corrupted(msg)` -> `Error::Corrupt(msg)`; everything else ->
/// `Error::Backend`.
pub(crate) fn backend(e: impl Into<redb::Error>) -> Error {
    match e.into() {
        redb::Error::Corrupted(m) => Error::Corrupt(m),
        other => Error::Backend(Box::new(other)),
    }
}

/// A stored value or key failed to decode.
pub(crate) fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corrupt(msg.into())
}

/// redb error met while opening/verifying the schema -> open error.
pub(crate) fn open_redb(e: impl Into<redb::Error>) -> OpenError {
    match e.into() {
        redb::Error::Corrupted(m) => OpenError::Corrupt(m),
        e @ (redb::Error::TableTypeMismatch { .. }
        | redb::Error::TypeDefinitionChanged { .. }
        | redb::Error::TableIsMultimap(_)
        | redb::Error::TableIsNotMultimap(_)) => OpenError::Corrupt(e.to_string()),
        redb::Error::DatabaseAlreadyOpen | redb::Error::LockProtocolMismatch => {
            OpenError::AlreadyOpen
        }
        redb::Error::UpgradeRequired(v) => OpenError::Unsupported(format!("redb file format {v}")),
        redb::Error::Io(e) => OpenError::Io(e),
        other => OpenError::Backend(Box::new(other)),
    }
}

/// `redb::DatabaseError` (from `Builder::create`) -> open error.
pub(crate) fn open_database(e: redb::DatabaseError) -> OpenError {
    match e {
        redb::DatabaseError::DatabaseAlreadyOpen | redb::DatabaseError::LockProtocolMismatch => {
            OpenError::AlreadyOpen
        }
        redb::DatabaseError::UpgradeRequired(v) => {
            OpenError::Unsupported(format!("redb file format {v}"))
        }
        redb::DatabaseError::Storage(redb::StorageError::Corrupted(m)) => OpenError::Corrupt(m),
        redb::DatabaseError::Storage(redb::StorageError::Io(e)) => {
            if e.kind() == std::io::ErrorKind::InvalidData {
                OpenError::Corrupt(e.to_string())
            } else {
                OpenError::Io(e)
            }
        }
        other => OpenError::Backend(Box::new(other)),
    }
}
