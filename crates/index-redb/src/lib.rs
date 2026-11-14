//! Embedded, persistent [`SecondaryIndex`] on `redb`.
//!
//! # Layout
//!
//! Every row of an entry lives under a **slot** (a `u64` from a global counter, never reused). A
//! slot is one (collection incarnation, generation, staging attempt). The collection record keeps
//! the list of live slots of its current incarnation; an entry is visible iff its slot is the slot
//! of the collection's active generation. `ReplaceGeneration`, `DiscardStaged` and
//! `DropCollection` only move slots to a `dead_slots` table (O(#generations)); [`purge`](masstuffy_index::SecondaryIndex::purge)
//! (`SecondaryIndex::purge`) later deletes their rows in bounded transactions, oldest dead slot
//! first. Queries resolve the active slot with one lookup and scan only that slot's key range.
//!
//! # Concurrency
//!
//! There is no lock of our own. redb serialises write transactions (`apply`, `purge`, `flush`) and
//! gives readers MVCC snapshots: a query never waits for the writer and never sees part of a batch.
//! Validation, URL massaging and value encoding happen before the write transaction starts. Every
//! method blocks: async callers use `spawn_blocking` (including [`RedbIndex::open`], which may run
//! a crash repair).
//!
//! # Durability
//!
//! By default every commit is fsynced (`sync_every = 1`, quick-repair on). With
//! `sync_every > 1`, intermediate commits are not durable (a crash loses a consistent suffix of
//! batches, which the engine replays from the CDX checkpoint); batches containing
//! `ReplaceGeneration`, `DiscardStaged` or `DropCollection` are always durable, so the engine may
//! delete files after they return.
//!
//! # Schema
//!
//! [`SCHEMA_VERSION`] covers table names, encodings and the `urlkey::massage` output the URL rows
//! were built with. A mismatch, or a file that is not a Masstuffy index, is reported by
//! [`OpenError::needs_rebuild`]: the engine deletes the file and rebuilds from the CDX files.

#![forbid(unsafe_code)]

mod codec;
mod error;
mod read;
mod schema;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod testutil;
mod write;

use std::fmt;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;

use masstuffy_index::{
    CollectionKey, CollectionState, Error, Hit, Op, Page, PatternQuery, Position, SecondaryIndex,
    UrlPattern, WarcDate,
};

pub use error::OpenError;

/// Version of the on-disk layout: table names, key and value encodings, and the
/// `urlkey::massage` output the URL rows were built with. Any change to these bumps it (a change
/// of the massaged form of a URL included: rows built with the old form would never match); a
/// mismatch at open means "rebuild".
pub const SCHEMA_VERSION: u64 = 1;

/// Tuning knobs. `#[non_exhaustive]`: build with `Options::default()` then set fields.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Options {
    /// redb page cache, bytes. Default 64 MiB (redb's own default of 1 GiB is too large to be
    /// implicit).
    pub cache_bytes: usize,
    /// Every `sync_every`-th commit (apply or purge) is durable (`Durability::Immediate`); the
    /// others use `Durability::None`. Batches containing `ReplaceGeneration`, `DiscardStaged` or
    /// `DropCollection` are always durable. Default 1 (every commit durable).
    pub sync_every: NonZeroU32,
    /// redb quick-repair on durable commits: the allocator state is saved and 2-phase commit is
    /// used, so reopening after a crash is near-instant instead of a full-file repair. Default
    /// true.
    pub quick_repair: bool,
    /// Verify every page checksum at open. Slow on big files; detects external corruption that
    /// would otherwise make redb panic. Default false.
    pub verify_on_open: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            cache_bytes: 64 << 20,
            sync_every: NonZeroU32::MIN,
            quick_repair: true,
            verify_on_open: false,
        }
    }
}

/// The redb-backed secondary index. `Send + Sync`; share it with `Arc`. Every method blocks (disk
/// I/O, fsync, and at open possibly a crash repair): async callers use `spawn_blocking`.
pub struct RedbIndex {
    db: redb::Database,
    sync_every: NonZeroU32,
    quick_repair: bool,
    /// Serialises `apply`, `purge` and `flush` from `begin_write` until after `unsynced` is updated.
    /// Lock order: this gate, then redb's internal writer lock (taken by `begin_write`); the gate is
    /// never held across anything else. It is uncontended in practice, since redb serialises
    /// writers anyway.
    gate: Mutex<()>,
    /// Non-durable commits since the last durable one. Only touched with `gate` held, so `Relaxed`
    /// ordering is enough.
    unsynced: AtomicU32,
}

impl fmt::Debug for RedbIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedbIndex")
            .field("sync_every", &self.sync_every)
            .field("quick_repair", &self.quick_repair)
            .finish_non_exhaustive()
    }
}

impl RedbIndex {
    /// Opens the index file at `path`, creating it (and initialising the schema) if it does not
    /// exist or is empty. The parent directory must exist. The file is locked by redb (whole-file
    /// lock) until the `RedbIndex` is dropped or closed.
    ///
    /// # Errors
    /// `OpenError::AlreadyOpen` if the file is open elsewhere (this process included);
    /// `SchemaMismatch` if it was written with another `SCHEMA_VERSION`; `Corrupt` if it is not a
    /// redb file, not a Masstuffy index, or fails redb's checks; `Unsupported` for a redb file
    /// format this redb cannot open; `Io`, `Backend` otherwise. `needs_rebuild()` tells which
    /// errors the engine fixes by deleting the file.
    pub fn open(path: impl AsRef<Path>, options: &Options) -> Result<RedbIndex, OpenError> {
        let db = schema::open_db(path.as_ref(), options)?;
        Ok(RedbIndex {
            db,
            sync_every: options.sync_every,
            quick_repair: options.quick_repair,
            gate: Mutex::new(()),
            unsynced: AtomicU32::new(0),
        })
    }

    /// Makes every commit so far durable (one empty `Immediate` commit if any commit since the last
    /// durable one used `Durability::None`; no-op otherwise).
    ///
    /// # Errors
    /// `Error::Backend` / `Error::Corrupt`.
    pub fn flush(&self) -> Result<(), Error> {
        write::flush(self)
    }

    /// Compacts the file (redb `compact`), returning whether anything was done. Needs exclusive
    /// access.
    ///
    /// # Errors
    /// `Error::Backend` / `Error::Corrupt`.
    pub fn compact(&mut self) -> Result<bool, Error> {
        self.db.compact().map_err(error::backend)
    }

    /// Closes the database, making pending non-durable commits durable, and reports any error
    /// (dropping does the same but ignores errors).
    ///
    /// # Errors
    /// `Error::Backend` / `Error::Corrupt`.
    pub fn close(self) -> Result<(), Error> {
        self.db.close().map_err(error::backend)
    }

    #[cfg(test)]
    pub(crate) fn db(&self) -> &redb::Database {
        &self.db
    }

    #[cfg(test)]
    pub(crate) fn unsynced(&self) -> u32 {
        self.unsynced.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl SecondaryIndex for RedbIndex {
    fn apply(&self, ops: &[Op<'_>]) -> Result<(), Error> {
        write::apply(self, ops)
    }

    fn state(&self, collection: CollectionKey) -> Result<Option<CollectionState>, Error> {
        read::state(self, collection)
    }

    fn collections(&self) -> Result<Vec<CollectionKey>, Error> {
        read::collections(self)
    }

    fn by_id(&self, scope: Option<CollectionKey>, record_id: &str) -> Result<Option<Hit>, Error> {
        read::by_id(self, scope, record_id)
    }

    fn by_url_nearest(
        &self,
        collection: CollectionKey,
        url: &str,
        date: Option<WarcDate>,
    ) -> Result<Option<Hit>, Error> {
        read::by_url_nearest(self, collection, url, date)
    }

    fn by_pattern(
        &self,
        collection: CollectionKey,
        pattern: &UrlPattern,
        query: &PatternQuery,
    ) -> Result<Page, Error> {
        read::by_pattern(self, collection, pattern, query)
    }

    fn list(
        &self,
        collection: CollectionKey,
        after: Option<Position>,
        limit: usize,
    ) -> Result<Vec<Hit>, Error> {
        read::list(self, collection, after, limit)
    }

    fn purge(&self, max_entries: usize) -> Result<usize, Error> {
        write::purge(self, max_entries)
    }

    fn stored_entries(&self) -> Result<u64, Error> {
        read::stored_entries(self)
    }
}
