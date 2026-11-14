//! Secondary-index contract for Masstuffy: the [`SecondaryIndex`] trait, a reference in-memory
//! implementation ([`MemIndex`]) and a reusable conformance suite (module `conformance`, cargo
//! feature `conformance`, macro `conformance_tests!`).
//!
//! The secondary index is a rebuildable search cache over the CDX of every collection, never a
//! source of truth.
//!
//! # Contract
//!
//! * **Atomic batches**: [`SecondaryIndex::apply`] takes a slice of [`Op`]s applied atomically and in
//!   order. A per-collection [`Checkpoint`] is an op, so it commits with the ops it covers.
//! * **Visibility (ACTIVE)**: an entry is visible iff its collection has an active generation equal
//!   to the entry's generation and it was not tombstoned or dropped.
//! * **Staging**: entries of a generation newer than the active one are accepted and invisible until
//!   `ReplaceGeneration` switches to it (an O(1) logical switch; older generations are retired).
//!   `DropCollection` is also an O(1) logical operation; a recreated collection never sees old rows.
//!   Retired and dropped rows are reclaimed by [`SecondaryIndex::purge`] in bounded steps.
//! * **Rebuild staging**: a rebuilt generation is staged with `Insert`s of the new generation; before
//!   restaging after an interrupted attempt, `DiscardStaged` clears the leftovers. There are two
//!   checkpoint slots: `checkpoint` (active generation) and `staged_checkpoint` (a staged generation),
//!   so the old generation's resume point survives staging; `ReplaceGeneration` carries them over.
//! * **Queries**: `by_id`, `by_url_nearest` (excluding `request`), `by_pattern` with cursor
//!   pagination, and an ordered `list` for I8 checks. Their exact semantics live in the [`rank`]
//!   module, shared by every backend.
//!
//! The crate does no I/O and has no async code: persistent backends live in other crates.

#![forbid(unsafe_code)]

#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
mod cursor;
mod error;
mod mem;
pub mod rank;
mod types;

pub use cursor::{Cursor, MAX_TOKEN_LEN};
pub use error::Error;
pub use mem::MemIndex;
pub use types::{
    Checkpoint, CollectionKey, CollectionState, Hit, Op, Page, PatternQuery, Position,
};

pub use masstuffy_urlkey::UrlPattern;
pub use masstuffy_warc::WarcDate;

/// Default page size of a pattern query (spec 4.3).
pub const DEFAULT_LIMIT: usize = 100;
/// Largest page size accepted by `by_pattern` and `list`.
pub const MAX_LIMIT: usize = 10_000;

/// Secondary index over the CDX of every collection (spec 1 "Index secondaire": a rebuildable search
/// cache, never a source of truth).
///
/// All methods are synchronous and may block (disk I/O in persistent backends): async callers use
/// `spawn_blocking`. The trait is object-safe (`Box<dyn SecondaryIndex>`).
///
/// Concurrency: any number of threads may call any method concurrently. Calls are linearizable:
/// `apply` calls are serialised internally, and every query observes the state after some prefix of
/// the applied batches, never part of a batch (snapshot isolation). No method holds a lock while
/// calling user code (there are no callbacks).
///
/// Durability: persistent backends must be atomic; durability on return is recommended but not
/// required, because a lost tail loses ops and checkpoint together and the engine replays the CDX
/// from the checkpoint.
pub trait SecondaryIndex: Send + Sync {
    /// Applies a batch atomically and in order (later ops see earlier ones). `apply(&[])` is a no-op.
    ///
    /// Insert/Tombstone/ReplaceGeneration/Checkpoint on an unknown collection create its state.
    ///
    /// # Errors
    /// `InvalidEntry`, `NotAnAdd`, `StaleGeneration` (with the index of the op), or a backend error.
    /// On error nothing changed.
    fn apply(&self, ops: &[Op<'_>]) -> Result<(), Error>;

    /// State of a collection, `None` if unknown.
    ///
    /// # Errors
    /// Backend errors.
    fn state(&self, collection: CollectionKey) -> Result<Option<CollectionState>, Error>;

    /// Known collections, ascending.
    ///
    /// # Errors
    /// Backend errors.
    fn collections(&self) -> Result<Vec<CollectionKey>, Error>;

    /// The most recently committed visible copy of `record_id` (greatest position within a
    /// collection; across collections the latest WARC-Date, then the greater collection key),
    /// restricted to `scope` if given.
    ///
    /// # Errors
    /// Backend errors.
    fn by_id(&self, scope: Option<CollectionKey>, record_id: &str) -> Result<Option<Hit>, Error>;

    /// The visible, non-`request` entry of `collection` for `url` nearest to `date` (latest if
    /// `None`). The URL is compared by massaged key for http(s), verbatim otherwise.
    ///
    /// # Errors
    /// Backend errors.
    fn by_url_nearest(
        &self,
        collection: CollectionKey,
        url: &str,
        date: Option<WarcDate>,
    ) -> Result<Option<Hit>, Error>;

    /// Visible entries of `collection` whose massaged key matches `pattern`, in `(key, date,
    /// position)` order, starting strictly after `query.after`. `Page::next` is `Some` iff at least
    /// one more candidate exists. Across a `ReplaceGeneration` a cursor degrades to "after this
    /// (key, date)", possibly with duplicates or skips at equal `(key, date)`.
    ///
    /// # Errors
    /// `InvalidLimit` if `query.limit` is outside `1..=MAX_LIMIT`; backend errors.
    fn by_pattern(
        &self,
        collection: CollectionKey,
        pattern: &UrlPattern,
        query: &PatternQuery,
    ) -> Result<Page, Error>;

    /// Visible entries of `collection` ordered by position, strictly after `after`, at most `limit`.
    ///
    /// # Errors
    /// `InvalidLimit` if `limit` is outside `1..=MAX_LIMIT`; backend errors.
    fn list(
        &self,
        collection: CollectionKey,
        after: Option<Position>,
        limit: usize,
    ) -> Result<Vec<Hit>, Error>;

    /// Reclaims up to `max_entries` permanently invisible stored entries (retired generations,
    /// dropped collections; never rows of a staged generation that has not been discarded) and returns how many. `0` means nothing is left.
    ///
    /// # Errors
    /// `InvalidLimit { limit: 0, max: usize::MAX }` if `max_entries == 0`; backend errors.
    fn purge(&self, max_entries: usize) -> Result<usize, Error>;

    /// Number of physically stored entries (visible + staged + not yet purged).
    ///
    /// # Errors
    /// Backend errors.
    fn stored_entries(&self) -> Result<u64, Error>;
}

#[cfg(test)]
mod mem_conformance {
    crate::conformance_tests!(crate::MemIndex::new);
}
