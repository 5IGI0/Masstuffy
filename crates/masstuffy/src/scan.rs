//! Full scan of a collection's live entries (spec 04 4.3 "Parcours complet").
//!
//! The scan reads the CDX, never the segments: bytes of a segment that no CDX line names
//! (orphans, invalidated zones) are never seen, and tombstoned records are skipped.

use std::sync::Arc;

use masstuffy_collection::{CollectionId, Entries, ReadError, Snapshot};

use crate::collection::{CollectionHandle, Shared};
use crate::error::Error;
use crate::read::{Fetch, Fetched, read_blocking};
use crate::repository::Core;
use crate::types::Located;

/// Largest value of [`ScanOptions::page_records`].
pub const MAX_SCAN_PAGE: usize = 10_000;
/// Largest value of [`ScanOptions::page_bytes`] (1 GiB).
const MAX_SCAN_PAGE_BYTES: u64 = 1 << 30;

/// Position in a scan: the generation plus the CDX offset of the next line to read. Opaque to
/// clients; its token is `"<generation>.<offset>"` (canonical decimal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCursor {
    generation: u32,
    next: u64,
}

impl ScanCursor {
    /// The textual form of this cursor.
    #[must_use]
    pub fn to_token(&self) -> String {
        format!("{}.{}", self.generation, self.next)
    }

    /// Parses a token produced by [`to_token`](Self::to_token).
    ///
    /// # Errors
    ///
    /// `InvalidQuery("scan cursor")` for anything that does not round-trip.
    pub fn from_token(token: &str) -> Result<ScanCursor, Error> {
        let bad = || Error::InvalidQuery("scan cursor");
        let (g, n) = token.split_once('.').ok_or_else(bad)?;
        let cursor = ScanCursor {
            generation: g.parse().map_err(|_| bad())?,
            next: n.parse().map_err(|_| bad())?,
        };
        if cursor.to_token() != token {
            return Err(bad());
        }
        Ok(cursor)
    }
}

/// Options of [`CollectionHandle::scan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOptions {
    /// Resume strictly after this cursor (`None` = from the first entry, the warcinfo).
    pub after: Option<ScanCursor>,
    /// Entries per page, `1..=MAX_SCAN_PAGE`. Default 1000.
    pub page_records: usize,
    /// A page ends once its fetched record bytes reach this (`1..=1 GiB`, default 16 MiB); a
    /// page always holds at least one entry. Ignored with `Fetch::None`.
    pub page_bytes: u64,
    /// What to fetch with each entry.
    pub fetch: Fetch,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            after: None,
            page_records: 1000,
            page_bytes: 16 << 20,
            fetch: Fetch::None,
        }
    }
}

/// One entry of a scan page.
#[derive(Debug)]
pub struct ScanItem {
    /// Where the record lives.
    pub located: Located,
    /// `None` with `Fetch::None`; else the record, or why it could not be read (`NotDecodable`,
    /// `Read`): a bad record does not stop the scan.
    pub record: Option<Result<Fetched, Error>>,
}

/// One page of a scan.
#[derive(Debug)]
pub struct ScanPage {
    /// The entries, in CDX order.
    pub items: Vec<ScanItem>,
    /// Resume strictly after this page.
    pub next: ScanCursor,
}

struct ScanState {
    snap: Arc<Snapshot>,
    entries: Entries,
}

/// A scan in progress. `Send`, not `Clone`.
pub struct Scan {
    core: Arc<Core>,
    shared: Arc<Shared>,
    opts: ScanOptions,
    /// The last cursor returned to the caller.
    cursor: Option<ScanCursor>,
    state: Option<Box<ScanState>>,
    done: bool,
}

struct PageOut {
    state: Box<ScanState>,
    generation: u32,
    items: Vec<ScanItem>,
    last_next: Option<u64>,
    exhausted: bool,
}

impl CollectionHandle {
    /// Starts a scan of the live entries of the CDX, tombstones applied, warcinfo included (spec
    /// 04 4.3 "Parcours complet"). No I/O happens until the first
    /// [`next_page`](Scan::next_page).
    ///
    /// A scan sees the CDX prefix captured when it (re)opens, so it is consistent with every put
    /// acknowledged before that moment, whatever the secondary index says. Records committed
    /// later are not seen by this scan object.
    ///
    /// Tombstones follow the same rule: a tombstone committed after the scan (re)opened does not
    /// hide entries this scan returns later, but it does once the scan reopens (after a cancelled
    /// page or an error). Entries already returned are never retracted.
    ///
    /// # Errors
    ///
    /// `InvalidQuery` (`page_records` outside `1..=MAX_SCAN_PAGE`, `page_bytes` outside
    /// `1..=1 GiB`), `ShuttingDown`, `UnknownCollection` (deleted).
    pub fn scan(&self, opts: ScanOptions) -> Result<Scan, Error> {
        if self.core.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::ShuttingDown);
        }
        self.shared.check_live()?;
        if opts.page_records == 0 || opts.page_records > MAX_SCAN_PAGE {
            return Err(Error::InvalidQuery("scan page_records"));
        }
        if opts.page_bytes == 0 || opts.page_bytes > MAX_SCAN_PAGE_BYTES {
            return Err(Error::InvalidQuery("scan page_bytes"));
        }
        Ok(Scan {
            core: Arc::clone(&self.core),
            shared: Arc::clone(&self.shared),
            cursor: opts.after,
            opts,
            state: None,
            done: false,
        })
    }
}

impl Scan {
    /// The next page, `Ok(None)` once exhausted. One read permit and one blocking job per page.
    ///
    /// # Cancellation
    ///
    /// Dropping the future loses nothing: the cursor only advances when a page is returned, so
    /// the next call reopens at the cursor (one more tombstone pass) and returns the same
    /// entries.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`; `UnknownCollection` (deleted); `StaleLocator` (the cursor's generation is no longer active, or a segment
    /// vanished because the generation changed: restart the scan); `InvalidQuery` (the cursor is
    /// not at a line start, or is beyond the CDX); `Collection` (CDX read error); `Join`.
    pub async fn next_page(&mut self) -> Result<Option<ScanPage>, Error> {
        use std::sync::atomic::Ordering;
        if self.done {
            return Ok(None);
        }
        if self.core.closed.load(Ordering::Acquire) {
            return Err(Error::ShuttingDown);
        }
        self.shared.check_live()?;
        let permit = Arc::clone(&self.core.read_sem)
            .acquire_owned()
            .await
            .map_err(|_| Error::ShuttingDown)?;
        let state = self.state.take();
        let cursor = self.cursor;
        let opts = self.opts;
        let core = Arc::clone(&self.core);
        let shared = Arc::clone(&self.shared);
        let out = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut ctx = core.read_ctxs.take();
            let r = page_blocking(&shared, state, cursor, &opts, &mut ctx);
            core.read_ctxs.give(ctx);
            r
        })
        .await
        .map_err(Error::Join)??;
        let PageOut {
            state,
            generation,
            items,
            last_next,
            exhausted,
        } = out;
        if let Some(next) = last_next {
            self.cursor = Some(ScanCursor { generation, next });
        }
        if exhausted {
            // Closes the CDX file.
            self.done = true;
        } else {
            self.state = Some(state);
        }
        match (items.is_empty(), self.cursor) {
            (false, Some(next)) => Ok(Some(ScanPage { items, next })),
            _ => Ok(None),
        }
    }

    /// The cursor after the last returned page (`after` before the first one).
    #[must_use]
    pub fn cursor(&self) -> Option<ScanCursor> {
        self.cursor
    }
}

/// Opens the entries at `cursor` (or at the start) on the current snapshot.
fn open_state(shared: &Shared, cursor: Option<ScanCursor>) -> Result<Box<ScanState>, Error> {
    let (snap, end) = shared.published.tail();
    let from = match cursor {
        Some(c) => {
            if snap.generation() != c.generation {
                return Err(Error::StaleLocator {
                    expected: snap.generation(),
                    got: c.generation,
                });
            }
            if c.next > end {
                return Err(Error::InvalidQuery("scan cursor"));
            }
            Some(c.next)
        }
        None => None,
    };
    let entries = snap
        .entries_from(from, end)
        .map_err(|source| match source {
            masstuffy_collection::Error::CdxRange { .. } => Error::InvalidQuery("scan cursor"),
            source => Error::Collection {
                collection: shared.id,
                source,
            },
        })?;
    Ok(Box::new(ScanState { snap, entries }))
}

fn page_blocking(
    shared: &Shared,
    state: Option<Box<ScanState>>,
    cursor: Option<ScanCursor>,
    opts: &ScanOptions,
    ctx: &mut masstuffy_collection::ReadCtx,
) -> Result<PageOut, Error> {
    let mut st = match state {
        Some(s) => s,
        None => open_state(shared, cursor)?,
    };
    let collection: CollectionId = shared.id;
    let generation = st.snap.generation();
    let mut items = Vec::with_capacity(opts.page_records.min(1024));
    let mut acc = 0u64;
    let mut last_next = None;
    let mut exhausted = false;
    while items.len() < opts.page_records {
        let entry = match st.entries.next_entry() {
            Ok(Some((_, e))) => e.into_owned(),
            Ok(None) => {
                exhausted = true;
                break;
            }
            Err(source) => return Err(Error::Collection { collection, source }),
        };
        // After `into_owned`: the borrow of the entry iterator has ended.
        let next = st.entries.position();
        let located = Located {
            collection,
            generation,
            entry,
        };
        let record = if opts.fetch == Fetch::None {
            None
        } else {
            match read_blocking(
                &st.snap,
                &shared.published,
                collection,
                &located.entry,
                opts.fetch,
                ctx,
            ) {
                Ok(f) => {
                    acc = acc.saturating_add(f.len());
                    Some(Ok(f))
                }
                Err(Error::Read {
                    source: ReadError::SegmentMissing { .. },
                    ..
                }) if shared.published.load().generation() != generation => {
                    // The generation changed under the scan.
                    return Err(Error::StaleLocator {
                        expected: shared.published.load().generation(),
                        got: generation,
                    });
                }
                Err(e) => Some(Err(e)),
            }
        };
        items.push(ScanItem { located, record });
        last_next = Some(next);
        if acc >= opts.page_bytes {
            break;
        }
    }
    Ok(PageOut {
        state: st,
        generation,
        items,
        last_next,
        exhausted,
    })
}
