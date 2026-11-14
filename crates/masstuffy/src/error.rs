//! Engine errors.

use std::path::PathBuf;
use std::sync::Arc;

use masstuffy_collection::{CollectionId, CommitError, ReadError};

use crate::config::ConfigError;

/// Errors of the engine.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The repository directory is locked by another process or handle.
    #[error("repository {} is locked by another process or handle", path.display())]
    Locked {
        /// The repository directory.
        path: PathBuf,
    },
    /// `init` on a directory that is not empty.
    #[error("{} is not empty: {}", dir.display(), entry.display())]
    NotEmpty {
        /// The directory.
        dir: PathBuf,
        /// The first offending entry.
        entry: PathBuf,
    },
    /// `init` on a directory that already holds a repository.
    #[error("{} is already a repository", path.display())]
    AlreadyInitialized {
        /// The directory.
        path: PathBuf,
    },
    /// `open` on a directory without `config.json`.
    #[error("{} is not a repository (no config.json)", path.display())]
    NotARepository {
        /// The directory.
        path: PathBuf,
    },
    /// A required directory of the layout is missing or not a directory.
    #[error("repository layout: {} is missing or not a directory", missing.display())]
    Layout {
        /// The missing path.
        missing: PathBuf,
    },
    /// `config.json` is invalid.
    #[error("config {}: {source}", path.display())]
    Config {
        /// The config file.
        path: PathBuf,
        /// Why.
        #[source]
        source: ConfigError,
    },
    /// An environment variable has an invalid value.
    #[error("invalid environment variable {var}={value:?}")]
    InvalidEnv {
        /// The variable.
        var: &'static str,
        /// Its value (lossy).
        value: String,
    },
    /// An I/O call failed.
    #[error("{op} failed on {}", path.display())]
    Io {
        /// The operation.
        op: &'static str,
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A durable filesystem primitive failed (`Locked` is mapped to [`Error::Locked`]).
    #[error(transparent)]
    Durable(masstuffy_durable::Error),
    /// The dictionary store failed.
    #[error("dictionary store: {0}")]
    Dict(#[from] masstuffy_dictstore::Error),
    /// The slug does not satisfy the slug policy.
    #[error("invalid slug {slug:?}")]
    InvalidSlug {
        /// The rejected slug.
        slug: String,
    },
    /// The slug is already used.
    #[error("slug {slug:?} is already used")]
    SlugTaken {
        /// The slug.
        slug: String,
    },
    /// The dictionary does not exist.
    #[error("dictionary {0} not found")]
    DictNotFound(u32),
    /// No such collection (slug or uuid as text).
    #[error("unknown collection {0}")]
    UnknownCollection(String),
    /// The locator's generation is not the active one.
    #[error("locator generation {got} is not the active generation {expected}")]
    StaleLocator {
        /// The active generation.
        expected: u32,
        /// The locator's generation.
        got: u32,
    },
    /// A collection-level operation failed.
    #[error("collection {collection}: {source}")]
    Collection {
        /// The collection.
        collection: CollectionId,
        /// The underlying error.
        #[source]
        source: masstuffy_collection::Error,
    },
    /// A record could not be rebuilt with its missing headers.
    #[error("record {index}: {source}")]
    Build {
        /// Index in the request.
        index: usize,
        /// The underlying error.
        #[source]
        source: masstuffy_warc::BuildError,
    },
    /// A record cannot be deleted (`CollectionHandle::delete`); nothing was written.
    #[error("record {index}: {source}")]
    Tombstone {
        /// Index in the (deduplicated) request.
        index: usize,
        /// The underlying error.
        #[source]
        source: masstuffy_collection::TombstoneError,
    },
    /// A record could not be prepared.
    #[error("record {index}: {source}")]
    Prepare {
        /// Index in the request.
        index: usize,
        /// The underlying error.
        #[source]
        source: masstuffy_collection::PrepareError,
    },
    /// The commit failed (nothing of the request is acknowledged).
    #[error("collection {collection}: commit failed: {source}")]
    Commit {
        /// The collection.
        collection: CollectionId,
        /// The underlying error.
        #[source]
        source: Arc<CommitError>,
    },
    /// The collection could not be reopened after a write failure.
    #[error("collection {collection}: reopen after a write failure failed: {source}")]
    Reopen {
        /// The collection.
        collection: CollectionId,
        /// The underlying error.
        #[source]
        source: Arc<masstuffy_collection::Error>,
    },
    /// A read failed.
    #[error("collection {collection}: read failed: {source}")]
    Read {
        /// The collection.
        collection: CollectionId,
        /// The underlying error.
        #[source]
        source: ReadError,
    },
    /// The record cannot be decoded as the indexed WARC record (use `get_raw`).
    #[error("record cannot be decoded as the indexed WARC record (use get_raw): {source}")]
    NotDecodable {
        /// The underlying error.
        #[source]
        source: ReadError,
    },
    /// The writer task stopped before replying: the request may or may not be durable.
    #[error("collection {0} is unavailable (writer stopped)")]
    CollectionUnavailable(CollectionId),
    /// The repository is shutting down or closed.
    #[error("repository is shutting down")]
    ShuttingDown,
    /// A background task failed.
    #[error("background task failed: {0}")]
    Join(#[source] tokio::task::JoinError),
    /// The shutdown of the repository failed (reported to every caller of `shutdown`).
    #[error("shutdown failed: {0}")]
    ShutdownFailed(Arc<Error>),
    /// The secondary index could not be opened (not fixed by recreating it).
    #[error("secondary index {}: {source}", path.display())]
    IndexOpen {
        /// The index file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Indexing failed for these collections (retried at their next commit or the next
    /// `sync_index`).
    #[error("secondary index lags for {} collection(s)", collections.len())]
    IndexLagging {
        /// The collections whose indexing failed.
        collections: Vec<CollectionId>,
    },
    /// The indexer task is not running.
    #[error("secondary index unavailable")]
    IndexUnavailable,
    /// A rebuild of this collection is already running.
    #[error("collection {0}: a rebuild is already running")]
    RebuildInProgress(CollectionId),
    /// The rebuild was abandoned before its commit (the old generation is still active).
    #[error("collection {collection}: rebuild aborted: {reason}")]
    RebuildAborted {
        /// The collection.
        collection: CollectionId,
        /// Why.
        reason: &'static str,
    },
    /// The rebuild failed. `committed`: the new generation is known to be active (the failure
    /// happened after the manifest swap, seen by a reopen); false means it is not, or unknown if
    /// the reopen failed too (then the next put or open decides).
    #[error("collection {collection}: rebuild failed: {source}")]
    Rebuild {
        /// The collection.
        collection: CollectionId,
        /// The new generation is known to be active.
        committed: bool,
        /// The underlying error.
        #[source]
        source: Box<masstuffy_collection::Error>,
    },
    /// Dictionary training failed.
    #[error("dictionary training failed: {0}")]
    Train(#[source] masstuffy_zframe::TrainError),
    /// An option is out of range (the field).
    #[error("invalid option: {0}")]
    InvalidOptions(&'static str),
    /// The collection changed generation repeatedly while a put was queued; nothing was written.
    #[error("collection {0}: generation changed while the request was queued; retry")]
    StaleGeneration(CollectionId),
    /// An internal invariant failed.
    #[error("internal error: {0}")]
    Internal(&'static str),
    /// Invalid query or option (limit, page size, cursor token, empty/oversize key).
    #[error("invalid query: {0}")]
    InvalidQuery(&'static str),
    /// The secondary index failed to answer.
    #[error("secondary index query failed: {0}")]
    IndexQuery(#[source] masstuffy_index::Error),
    /// The raw-import envelope is malformed or unreadable.
    #[error("raw import envelope: {0}")]
    Envelope(#[source] masstuffy_cdx::EnvelopeError),
    /// A stream import stopped; `report` says what is durable.
    #[error("import stopped after {} records (resume at byte {}): {source}", report.records, report.committed_offset)]
    Import {
        /// Progress of the import up to the failure.
        report: Box<crate::import::ImportReport>,
        /// Why it stopped.
        #[source]
        source: Box<Error>,
    },
}

/// Coarse classification of an [`Error`], for API layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The named thing does not exist.
    NotFound,
    /// The operation conflicts with the current state.
    Conflict,
    /// On-disk data is corrupt.
    Corrupt,
    /// The input is invalid.
    Invalid,
    /// An I/O failure.
    Io,
    /// The service cannot answer now (shutdown, stopped task).
    Unavailable,
}

impl Error {
    /// The coarse kind of this error.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        use masstuffy_collection::Error as C;
        match self {
            Error::UnknownCollection(_) | Error::NotARepository { .. } | Error::DictNotFound(_) => {
                ErrorKind::NotFound
            }
            Error::Locked { .. }
            | Error::NotEmpty { .. }
            | Error::AlreadyInitialized { .. }
            | Error::SlugTaken { .. }
            | Error::StaleLocator { .. }
            | Error::RebuildInProgress(_)
            | Error::RebuildAborted { .. }
            | Error::StaleGeneration(_) => ErrorKind::Conflict,
            Error::Rebuild { source, .. } => match &**source {
                C::TombstoneDuringRebuild { .. } | C::StaleSource(_) | C::RebuildCancelled => {
                    ErrorKind::Conflict
                }
                C::RebuildRead { .. } | C::LocatorOrder { .. } | C::CdxCorrupt { .. } => {
                    ErrorKind::Corrupt
                }
                C::Resolve(_) => ErrorKind::NotFound,
                _ => ErrorKind::Io,
            },
            Error::Layout { .. } | Error::NotDecodable { .. } | Error::Reopen { .. } => {
                ErrorKind::Corrupt
            }
            Error::Read { source, .. } => match source {
                ReadError::DigestMismatch { .. }
                | ReadError::Frame(_)
                | ReadError::Codec(_)
                | ReadError::Record(_)
                | ReadError::EntryMismatch(_) => ErrorKind::Corrupt,
                _ => ErrorKind::Io,
            },
            Error::Collection { source, .. } => match source {
                C::Manifest { .. } | C::CdxCorrupt { .. } | C::I2Violation { .. } => {
                    ErrorKind::Corrupt
                }
                _ => ErrorKind::Io,
            },
            Error::Dict(e) => match e {
                masstuffy_dictstore::Error::Io { .. } | masstuffy_dictstore::Error::Durable(_) => {
                    ErrorKind::Io
                }
                _ => ErrorKind::Corrupt,
            },
            Error::IndexQuery(e) => match e {
                masstuffy_index::Error::Corrupt(_) => ErrorKind::Corrupt,
                _ => ErrorKind::Io,
            },
            Error::Envelope(e) => match e {
                masstuffy_cdx::EnvelopeError::Io(_) => ErrorKind::Io,
                _ => ErrorKind::Invalid,
            },
            Error::Import { source, .. } => source.kind(),
            Error::InvalidQuery(_)
            | Error::Config { .. }
            | Error::InvalidEnv { .. }
            | Error::InvalidSlug { .. }
            | Error::Train(_)
            | Error::InvalidOptions(_)
            | Error::Build { .. }
            | Error::Tombstone { .. }
            | Error::Prepare { .. } => ErrorKind::Invalid,
            Error::Commit { source, .. }
                if matches!(**source, CommitError::TombstoneNotCommitted { .. }) =>
            {
                ErrorKind::Invalid
            }
            Error::Io { .. }
            | Error::Durable(_)
            | Error::Commit { .. }
            | Error::IndexOpen { .. } => ErrorKind::Io,
            Error::ShuttingDown
            | Error::CollectionUnavailable(_)
            | Error::Join(_)
            | Error::ShutdownFailed(_)
            | Error::IndexLagging { .. }
            | Error::IndexUnavailable
            | Error::Internal(_) => ErrorKind::Unavailable,
        }
    }
}

impl From<masstuffy_durable::Error> for Error {
    fn from(e: masstuffy_durable::Error) -> Error {
        match e {
            masstuffy_durable::Error::Locked { .. } => Error::Locked {
                path: e.path().to_path_buf(),
            },
            e => Error::Durable(e),
        }
    }
}
