//! Public data types of the engine.

use std::path::PathBuf;
use std::sync::Arc;

use masstuffy_cdx::{Entry, ImportMeta};
use masstuffy_collection::{CdxCounts, Codec, CollectionId, RawEncoding, RecoveryReport};

/// Where a committed record lives. Self-describing: enough to read it back with
/// `Repository::get` and `Repository::get_raw`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// The collection.
    pub collection: CollectionId,
    /// The generation the entry belongs to; a rebuild invalidates older locators.
    pub generation: u32,
    /// The index entry (segment, offset, size, digest, identity).
    pub entry: Entry<'static>,
}

/// Names a loaded collection (by slug or by uuid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionRef<'a> {
    /// The current slug.
    Slug(&'a str),
    /// The uuid.
    Id(CollectionId),
}

impl<'a> From<&'a str> for CollectionRef<'a> {
    fn from(s: &'a str) -> Self {
        CollectionRef::Slug(s)
    }
}

impl From<CollectionId> for CollectionRef<'_> {
    fn from(id: CollectionId) -> Self {
        CollectionRef::Id(id)
    }
}

/// What `Repository::delete_collection` did.
#[derive(Debug, Clone)]
pub struct DeletedCollection {
    /// The collection uuid.
    pub id: CollectionId,
    /// The slug it had (now free, unless `slug_reserved`).
    pub slug: Arc<str>,
    /// The trash directory was removed (false: the next open sweeps it).
    pub trash_removed: bool,
    /// The parent fsync after the trash rename failed: the deletion may be undone by a crash, so
    /// the slug stays reserved until the repository is reopened.
    pub slug_reserved: bool,
    /// The index rows were dropped (false: index failure; dropped by the orphan GC at the next
    /// pass, `rebuild_index` or open).
    pub index_dropped: bool,
}

/// Most locators accepted by one `CollectionHandle::delete` call.
pub const MAX_DELETE_TARGETS: usize = 65_536;

/// One delegated record for `put_raw`.
#[derive(Debug)]
pub struct RawItem {
    /// What the client claims about the payload.
    pub meta: ImportMeta<'static>,
    /// The stored bytes (one WARC record, or one zstd frame in a Zstd collection).
    pub payload: Vec<u8>,
}

/// What `get_raw` returns.
#[derive(Debug)]
pub struct RawRecord {
    /// The stored bytes.
    pub bytes: Vec<u8>,
    /// Their encoding.
    pub encoding: RawEncoding,
}

/// Summary of one collection.
#[derive(Debug, Clone)]
pub struct CollectionInfo {
    /// The collection uuid.
    pub id: CollectionId,
    /// The slug.
    pub slug: Arc<str>,
    /// The active generation's codec.
    pub codec: Codec,
    /// The active generation.
    pub generation: u32,
    /// Size of the active generation, read together with `generation`.
    pub size: CollectionSize,
}

/// Size of a collection's active generation (see `docs/http-api.md`, "Collection size"). Derived
/// from exact CDX counters kept by the writer task: reading it is O(1), no I/O.
///
/// If the writer task loses its writer (failed reopen), the last published value stays until the
/// writer is back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollectionSize {
    /// Live records: `records_total - tombstones` (saturating). Exact iff every tombstone names a
    /// distinct record; lower after a record was deleted twice or after a delete with a crafted
    /// locator; exact again after a rebuild.
    pub records: u64,
    /// `Add` lines minus the generation's own warcinfo (deleted records included until a rebuild;
    /// warcinfo records of earlier generations copied by a rebuild included).
    pub records_total: u64,
    /// `T` lines (duplicates included; 0 after a rebuild).
    pub tombstones: u64,
    /// Sum of `raw_size` of the `Add` lines (stored, i.e. compressed, bytes; warcinfo included).
    pub stored_bytes: u64,
    /// Sum of `raw_size` of the `T` lines (same caveat as `records`).
    pub tombstoned_bytes: u64,
    /// Total length of the active generation's segment files (orphan tails included).
    pub segment_bytes: u64,
    /// Length of the active CDX file (fsynced, header included).
    pub cdx_bytes: u64,
}

impl CollectionSize {
    /// Builds the size from a writer's counters.
    pub(crate) fn from_counts(c: CdxCounts, segment_bytes: u64, cdx_bytes: u64) -> CollectionSize {
        let records_total = c.adds.saturating_sub(1);
        CollectionSize {
            records: records_total.saturating_sub(c.tombstones),
            records_total,
            tombstones: c.tombstones,
            stored_bytes: c.add_bytes,
            tombstoned_bytes: c.tombstone_bytes,
            segment_bytes,
            cdx_bytes,
        }
    }
}

/// Counters of one writer task (monotonic since open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterStats {
    /// Successful commits (one per group, plus one per request in the isolation fallback).
    pub commits: u64,
    /// Records acknowledged.
    pub records: u64,
    /// Length of the fsynced CDX after the last successful commit.
    pub cdx_len: u64,
}

/// At most this many unknown entries are listed in an [`OpenReport`] (the count is exact).
pub const MAX_REPORTED_UNKNOWN: usize = 64;

/// Startup report (libraries return data; the engine also logs it).
#[derive(Debug)]
pub struct OpenReport {
    /// What the dictionary store scan ignored.
    pub dicts: masstuffy_dictstore::ScanReport,
    /// Collections that were loaded.
    pub loaded: Vec<LoadedCollection>,
    /// Collections that were skipped.
    pub skipped: Vec<SkippedCollection>,
    /// Aborted creates (uuid-named directory without manifest) that were trashed.
    pub trashed: Vec<PathBuf>,
    /// Entries of `data/repository` that are not collections (kept), at most
    /// [`MAX_REPORTED_UNKNOWN`].
    pub unknown: Vec<PathBuf>,
    /// Exact number of unknown entries.
    pub unknown_count: u64,
    /// `.tmp-*` entries removed from the repository root.
    pub tmp_removed: usize,
    /// `.trash-*` entries removed from `data/repository`.
    pub trash_removed: usize,
    /// The secondary index file was unreadable or of another schema and was recreated empty:
    /// every collection is being re-indexed.
    pub index_recreated: bool,
}

/// A collection loaded at open.
#[derive(Debug)]
pub struct LoadedCollection {
    /// The collection uuid.
    pub id: CollectionId,
    /// The slug.
    pub slug: Arc<str>,
    /// What recovery did.
    pub recovery: RecoveryReport,
}

/// A collection directory that was not loaded.
#[derive(Debug)]
pub struct SkippedCollection {
    /// The directory.
    pub dir: PathBuf,
    /// Why.
    pub reason: SkipReason,
}

/// Why a collection was skipped at open.
#[derive(Debug)]
#[non_exhaustive]
pub enum SkipReason {
    /// `Collection::open` failed (anything but `ManifestMissing`).
    Open(masstuffy_collection::Error),
    /// The manifest uuid differs from the directory name.
    UuidMismatch {
        /// The uuid found in the manifest.
        manifest: CollectionId,
    },
    /// Several loadable collections share this slug (I7 violated on disk): all are skipped.
    DuplicateSlug {
        /// The slug.
        slug: String,
    },
    /// `ManifestMissing` but trashing the directory failed (it stays, reported).
    TrashFailed(masstuffy_durable::Error),
}
