//! The Masstuffy repository engine: the only async crate of the workspace.
//!
//! A [`Repository`] is one directory:
//!
//! ```text
//! <dir>/
//! ├── .lock                 directory lock, held for the lifetime of the repository
//! ├── config.json           versioned config (see [`Config`]); its atomic write commits `init`
//! └── data/
//!     ├── buffer/           created by init, unused so far
//!     ├── dict/zstd/        dictionary store
//!     └── repository/<uuid>/   one collection each
//! ```
//!
//! # Async boundary
//!
//! Every blocking call (file I/O, fsync, zstd, dictionary loading, collection open and create)
//! runs in `spawn_blocking`. Callers prepare (serialise and compress) their records in the
//! blocking pool, then queue them on the bounded channel of the collection's writer task, which
//! merges queued requests into one commit (one data fsync and one index fsync) and answers each
//! request once its records are durable. Readers load an `Arc<Snapshot>` and use positioned
//! reads: they never wait for writers.
//!
//! # Lock order
//!
//! `gate`, then `create_lock`, then `index_rebuild`, then per-collection `admin` mutexes
//! (ascending uuid), then the registry, then snapshot and pool mutexes, the indexer hub, the
//! index handle and the indexer task handle (all leaves). No std lock is held across an
//! `.await`.
//!
//! # Secondary index
//!
//! A single indexer task keeps a secondary index (`data/index.redb` with the default cargo
//! feature `redb`, an in-memory index otherwise) equal to the CDX of every loaded collection. It
//! tails the fsynced CDX of each collection from its index checkpoint, so `put` replies after the
//! CDX fsync and never waits for the index: the index is eventually consistent, and
//! [`Repository::sync_index`] is the barrier that gives read-your-writes.
//! [`Repository::rebuild_index`] re-indexes everything from the CDX while puts continue.
#![forbid(unsafe_code)]

mod collection;
mod config;
mod error;
mod index;
#[cfg(test)]
mod index_tests;
mod indexer;
mod layout;
mod pool;
mod repository;
mod resolver;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod testutil;
mod types;
mod writer;

pub use collection::CollectionHandle;
pub use config::{Config, ConfigError, MAX_CONFIG_BYTES, RAW_IMPORT_CHECK_ENV};
pub use error::{Error, ErrorKind};
pub use masstuffy_cdx::{Entry, ImportMeta};
pub use masstuffy_collection::{Codec, CollectionId, RawCheck, RawEncoding, RecoveryReport};
pub use masstuffy_dictstore::ScanReport;
pub use masstuffy_warc::{Record, RecordBuilder, WarcDate};
pub use repository::Repository;
pub use types::{
    CollectionInfo, LoadedCollection, Located, MAX_REPORTED_UNKNOWN, OpenReport, RawItem,
    RawRecord, SkipReason, SkippedCollection, WriterStats,
};
