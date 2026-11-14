//! The Masstuffy repository engine: the only async crate of the workspace.
//!
//! A [`Repository`] is one directory:
//!
//! ```text
//! <dir>/
//! ├── .lock                 directory lock, held for the lifetime of the repository
//! ├── config.json           versioned config (see [`Config`]); its atomic write commits `init`
//! └── data/
//!     ├── buffer/           created by init, unused ([LEGACY] spill and lock of the old training)
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
//! `gate`, then `index_rebuild`, then a `rebuild_sem` permit (rebuild units only), then
//! per-collection `admin` mutexes (ascending uuid), then the per-collection `admission` lock,
//! then `create_lock` (it moved after `admin` in M21: rename holds both), then the registry, then
//! the leaves: the slug lock (only under the registry write lock), the per-collection rebuild
//! slot mutex (held during blocking I/O by design, nothing else is taken meanwhile), snapshot
//! and pool mutexes, the indexer hub, the index handle and the indexer task handle. `train_sem`
//! is taken with no other lock held. No std lock is held across an `.await`; the writer task
//! never takes the admin mutex or the gate. The admin operations never take the gate either:
//! they check `is_closing` after acquiring `admin`.
//!
//! # Admin operations
//!
//! [`Repository::delete_collection`] moves the collection directory to the trash (the commit
//! point), frees the slug and drops the index rows; [`Repository::rename_collection`] rewrites
//! the manifest slug only; [`CollectionHandle::delete`] and
//! [`delete_by_id`](CollectionHandle::delete_by_id) delete records logically with tombstones
//! (the bytes stay until the next rebuild; a read by explicit locator still returns them).
//! They are serialised per collection (admin mutex) with rebuilds and each other. A deleted
//! collection answers `UnknownCollection` on every handle operation. See the `admin` module docs
//! for the protocols and the crash table.
//!
//! # Secondary index
//!
//! A single indexer task keeps a secondary index (`data/index.redb` with the default cargo
//! feature `redb`, an in-memory index otherwise) equal to the CDX of every loaded collection. It
//! tails the fsynced CDX of each collection from its index checkpoint, so `put` replies after the
//! CDX fsync and never waits for the index: the index is eventually consistent, and
//! [`Repository::sync_index`] is the barrier that gives read-your-writes.
//! [`Repository::rebuild_index`] re-indexes everything from the CDX while puts continue.
//!
//! # Queries
//!
//! [`CollectionHandle::search`], `find_by_url`/`get_by_url` and [`Repository::find_by_id`] /
//! `get_by_id` answer from the secondary index and are therefore eventually consistent (call
//! `sync_index` first for read-your-writes). [`CollectionHandle::scan`] reads the CDX itself and
//! is consistent with every put acknowledged before the scan (re)opens.
//! [`CollectionHandle::put_raw_envelope`] bulk-imports an envelope stream.
//!
//! # Dictionary training and rebuild
//!
//! [`CollectionHandle::train_dictionary`] trains a zstd dictionary from a deterministic sample of
//! the live records and installs it in the repository store.
//! [`CollectionHandle::rebuild`] moves a collection to a new generation with a target codec while
//! puts and reads continue: the manifest swap is the single commit point, the secondary index
//! stages the new generation and activates it afterwards, and locators of the replaced generation
//! stay readable until the index has switched (then `StaleLocator`). Requests queued for the old
//! generation at the switch are prepared again for the new one. If the index wait after a rebuild
//! fails (`index_switched == false`), old-generation hits return `StaleLocator` until the index
//! catches up. Shutdown cancels both; a crash
//! at any point is repaired by `Repository::open` and the indexer. See the `rebuild` module docs.
#![forbid(unsafe_code)]

mod admin;
#[cfg(test)]
mod admin_tests;
mod collection;
mod config;
mod error;
mod import;
mod index;
#[cfg(test)]
mod index_tests;
mod indexer;
mod layout;
mod pool;
mod query;
#[cfg(test)]
mod query_tests;
mod read;
mod rebuild;
#[cfg(test)]
mod rebuild_tests;
mod repository;
mod resolver;
mod scan;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod testutil;
mod train;
mod types;
mod writer;

pub use collection::CollectionHandle;
pub use config::{Config, ConfigError, MAX_CONFIG_BYTES, RAW_IMPORT_CHECK_ENV};
pub use error::{Error, ErrorKind};
pub use import::{
    ImportOptions, ImportReport, MAX_IMPORT_BATCH_RECORDS, MAX_REPORTED_REJECTIONS, OnInvalid,
    Rejection,
};
pub use masstuffy_cdx::{Entry, EnvelopeFormat, ImportMeta};
pub use masstuffy_collection::{Codec, CollectionId, RawCheck, RawEncoding, RecoveryReport};
pub use masstuffy_collection::{
    RebuildReport as CopyReport, SkipReason as RecordSkipReason, SkippedRecord, Unreadable,
};
pub use masstuffy_dictstore::ScanReport;
pub use masstuffy_urlkey::{HostMatch, PathFilter, PatternError, PortFilter, UrlPattern};
pub use masstuffy_warc::{Record, RecordBuilder, WarcDate};
pub use query::{
    DEFAULT_SEARCH_LIMIT, MAX_QUERY_LEN, MAX_SEARCH_LIMIT, SearchCursor, SearchPage, SearchQuery,
};
pub use read::{Fetch, Fetched};
pub use rebuild::{RebuildOptions, RebuildReport};
pub use repository::Repository;
pub use scan::{MAX_SCAN_PAGE, Scan, ScanCursor, ScanItem, ScanOptions, ScanPage};
pub use train::{TrainOptions, TrainReport};
pub use types::{
    CollectionInfo, CollectionRef, DeletedCollection, LoadedCollection, Located,
    MAX_DELETE_TARGETS, MAX_REPORTED_UNKNOWN, OpenReport, RawItem, RawRecord, SkipReason,
    SkippedCollection, WriterStats,
};
