//! Plain data types of the index contract.

use std::fmt;

use masstuffy_cdx::Entry;

use crate::cursor::Cursor;
use crate::{DEFAULT_LIMIT, Error, MAX_LIMIT};

/// Collection identity inside the index: the 128-bit value of the collection uuid
/// (the engine converts with `CollectionKey::new(collection_id.as_u128())`).
/// `index` does not depend on `collection`, hence its own newtype.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CollectionKey(u128);

impl CollectionKey {
    /// Wraps a 128-bit collection id.
    #[must_use]
    pub const fn new(v: u128) -> CollectionKey {
        CollectionKey(v)
    }

    /// The 128-bit value.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0
    }
}

impl From<u128> for CollectionKey {
    fn from(v: u128) -> Self {
        CollectionKey(v)
    }
}

/// Canonical lowercase hyphenated uuid form, used in error messages.
impl fmt::Display for CollectionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            v >> 96,
            (v >> 80) & 0xffff,
            (v >> 64) & 0xffff,
            (v >> 48) & 0xffff,
            v & 0xffff_ffff_ffff
        )
    }
}

/// Locator position of an entry inside one generation of one collection. `Ord` = (segment, offset)
/// = CDX order = commit order (the single writer appends in locator order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Position {
    /// Segment number.
    pub segment: u32,
    /// Byte offset in the segment.
    pub offset: u64,
}

/// "The CDX of `generation` is reflected in the index up to byte `cdx_offset`."
///
/// `cdx_offset` is the byte offset in `index.<generation>.cdx` just after the last line whose op is
/// in the index, i.e. where the CDX reader must resume. The engine only passes fsynced, line-aligned
/// offsets; the index stores the value verbatim and never interprets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    /// Generation of the CDX file.
    pub generation: u32,
    /// Byte offset just after the last applied line.
    pub cdx_offset: u64,
}

/// Per-collection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CollectionState {
    /// Visible generation. `None` until the first `ReplaceGeneration`: nothing is visible.
    pub active_generation: Option<u32>,
    /// Resume point of the active generation (or of any generation while none is active).
    pub checkpoint: Option<Checkpoint>,
    /// Resume point of a staged generation (generation > active), e.g. a rebuild being indexed.
    pub staged_checkpoint: Option<Checkpoint>,
}

/// A visible (ACTIVE) entry: enough to read the record and to answer metadata without reading it.
/// `entry.kind` is always `Kind::Add`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Collection holding the entry.
    pub collection: CollectionKey,
    /// Generation of the entry (the active one).
    pub generation: u32,
    /// The CDX entry.
    pub entry: Entry<'static>,
}

impl Hit {
    /// `(entry.segment, entry.offset)`.
    #[must_use]
    pub fn position(&self) -> Position {
        Position {
            segment: self.entry.segment,
            offset: self.entry.offset,
        }
    }
}

/// One mutation. A batch (`&[Op]`) is applied atomically and in order (later ops see earlier ones).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op<'a> {
    /// Add one CDX `A` line of `generation`. `entry.kind` must be `Add` and `entry.validate()` must
    /// pass. Identity = (collection, generation, segment, offset). If that identity already exists,
    /// this is a no-op (first wins; the same locator always holds the same record, so replays are
    /// idempotent). The same record_id at another locator is a distinct copy.
    Insert {
        /// Target collection.
        collection: CollectionKey,
        /// Generation of the CDX line.
        generation: u32,
        /// The entry.
        entry: Entry<'a>,
    },
    /// Remove the entry with that identity (a CDX `T` line). Absent -> no-op.
    Tombstone {
        /// Target collection.
        collection: CollectionKey,
        /// Generation of the CDX line.
        generation: u32,
        /// Locator of the entry to remove.
        position: Position,
    },
    /// Make `generation` the active generation. Entries of older generations become invisible
    /// immediately (retired: reclaimed by `purge`, or eagerly). Entries of newer generations stay
    /// staged. `generation == active` -> no-op. `generation < active` -> `StaleGeneration`.
    /// Creates the collection state if missing. Checkpoint slots: the new `checkpoint` is the old one
    /// if its generation is `generation`, else `staged_checkpoint` if its generation is `generation`,
    /// else `None`; `staged_checkpoint` is kept only if its generation is greater.
    ReplaceGeneration {
        /// Target collection.
        collection: CollectionKey,
        /// Generation to activate.
        generation: u32,
    },
    /// Remove every entry of the staged `generation` (generation > active, or no active generation),
    /// e.g. the leftovers of an interrupted rebuild attempt that will reuse the same generation
    /// number. Logically O(1): the entries are invisible to everything at once (including a later
    /// `ReplaceGeneration(generation)` and later Inserts at the same positions, which start fresh),
    /// and their storage is reclaimed eagerly or by `purge`. Also clears `staged_checkpoint` if its
    /// generation equals `generation`. Unknown collection -> no-op (no state created).
    /// `generation <= active` -> `Error::NotStaged`.
    ///
    /// Engine contract: before restaging generation g (rebuild retry), send `DiscardStaged { g }`.
    DiscardStaged {
        /// Target collection.
        collection: CollectionKey,
        /// Staged generation to clear.
        generation: u32,
    },
    /// Forget the collection: state (active generation, both checkpoints) removed, all its entries
    /// invisible immediately, storage reclaimed eagerly or by `purge`. Later ops on the same key
    /// start from a clean state: entries from before the drop never reappear, even with the same
    /// generation and position. Unknown collection -> no-op.
    DropCollection {
        /// Collection to drop.
        collection: CollectionKey,
    },
    /// Set a checkpoint (last write wins, no monotonicity check: the engine owns it). Routing: no
    /// active generation or `checkpoint.generation == active` -> `checkpoint`; greater than active ->
    /// `staged_checkpoint` (overwriting one of another generation); lower than active ->
    /// `StaleGeneration`. Creates the collection state if missing.
    ///
    /// While no generation is active every checkpoint lands in the main slot, so a later
    /// `ReplaceGeneration` to another generation drops it. The engine activates generation 1 in the
    /// same batch that creates a collection, so this never loses a resume point in practice.
    Checkpoint {
        /// Target collection.
        collection: CollectionKey,
        /// New checkpoint.
        checkpoint: Checkpoint,
    },
}

/// Options of a pattern query.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PatternQuery {
    /// Page size, `1..=MAX_LIMIT`.
    pub limit: usize,
    /// Start strictly after this cursor.
    pub after: Option<Cursor>,
}

impl PatternQuery {
    /// Query with that page size and no cursor. The limit is validated by the index, not here.
    #[must_use]
    pub fn new(limit: usize) -> PatternQuery {
        PatternQuery { limit, after: None }
    }

    /// Sets the cursor to continue after.
    #[must_use]
    pub fn after(mut self, cursor: Cursor) -> PatternQuery {
        self.after = Some(cursor);
        self
    }
}

impl Default for PatternQuery {
    fn default() -> Self {
        PatternQuery::new(DEFAULT_LIMIT)
    }
}

/// One page of a pattern query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Hits in pattern order.
    pub hits: Vec<Hit>,
    /// `Some(cursor of the last hit)` iff at least one more candidate exists after it.
    pub next: Option<Cursor>,
}

/// Checks `1..=MAX_LIMIT`.
pub(crate) fn check_limit(limit: usize) -> Result<(), Error> {
    if (1..=MAX_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(Error::InvalidLimit {
            limit,
            max: MAX_LIMIT,
        })
    }
}
