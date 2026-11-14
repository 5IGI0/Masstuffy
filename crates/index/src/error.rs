//! Errors of the secondary index.

use crate::types::CollectionKey;

/// Error of a [`SecondaryIndex`](crate::SecondaryIndex) call.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// `Insert` entry fails `Entry::validate`.
    #[error("op {op}: invalid entry")]
    InvalidEntry {
        /// Index of the op in the batch.
        op: usize,
        /// Validation failure.
        #[source]
        source: masstuffy_cdx::EntryError,
    },
    /// `Insert` whose entry has `kind == Tombstone`.
    #[error("op {op}: Insert carries a tombstone entry")]
    NotAnAdd {
        /// Index of the op in the batch.
        op: usize,
    },
    /// Insert/Tombstone/ReplaceGeneration/Checkpoint targeting a generation older than the active one.
    #[error(
        "op {op}: generation {generation} of collection {collection} is older than the active generation {active}"
    )]
    StaleGeneration {
        /// Index of the op in the batch.
        op: usize,
        /// Collection concerned.
        collection: CollectionKey,
        /// Generation of the op.
        generation: u32,
        /// Active generation of the collection at that point of the batch.
        active: u32,
    },
    /// `DiscardStaged` of a generation that is not staged (`generation <= active`).
    #[error(
        "op {op}: generation {generation} of collection {collection} is not staged (active {active})"
    )]
    NotStaged {
        /// Index of the op in the batch.
        op: usize,
        /// Collection concerned.
        collection: CollectionKey,
        /// Generation of the op.
        generation: u32,
        /// Active generation of the collection at that point of the batch.
        active: u32,
    },
    /// `limit`/`max_entries` out of range.
    #[error("limit {limit} out of range 1..={max}")]
    InvalidLimit {
        /// Offending value.
        limit: usize,
        /// Largest accepted value.
        max: usize,
    },
    /// Malformed or non-canonical cursor token.
    #[error("invalid cursor token")]
    InvalidCursor,
    /// An internal lock was poisoned by a panic (MemIndex).
    #[error("index lock poisoned")]
    Poisoned,
    /// Stored data failed to decode (persistent backends).
    #[error("corrupt index: {0}")]
    Corrupt(String),
    /// Backend failure (I/O, database).
    #[error("index backend error")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}
