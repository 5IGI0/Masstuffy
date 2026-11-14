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
//! `gate`, then `create_lock`, then per-collection `admin` mutexes (ascending uuid), then the
//! registry, then snapshot and pool mutexes. No std lock is held across an `.await`.
#![forbid(unsafe_code)]

mod collection;
mod config;
mod error;
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
