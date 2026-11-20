//! Grep: a shared, circular scan of one or more collections (spec 04 4.3 "Parcours complet").
//!
//! Per collection, at most one sequential pass over the CDX runs at any time, whatever the number
//! of greps attached to it. A new grep attaches at the scan's current CDX position, receives every
//! record from there to the end, then the scan wraps to the start and continues until the late
//! joiner has seen what it missed. Each record is read, digest-checked and decoded once per pass
//! and matched by every grep that needs it. The scanner stops when no grep is attached.
//!
//! The traversal is driven by the CDX, never by the segments: orphan and invalidated zones are
//! skipped and tombstones applied.
//!
//! # Coverage
//!
//! A grep covers exactly the live records of the CDX prefix `[header, T)`, where `T` is the
//! fsynced CDX length when the grep attaches: every record acknowledged before the call. Records
//! appended later are never returned, records deleted before the call are never returned, and a
//! record deleted during the grep may still be returned (a hit is never retracted).
//!
//! # Failure and back-pressure
//!
//! A rebuild (generation change) during a grep fails that collection's part with `StaleLocator`;
//! the other collections continue. The scanner never waits for a consumer: when a grep's channel
//! is full, that grep is detached with `GrepLagged` so that one slow client cannot stall the
//! shared scan.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

use masstuffy_collection::CollectionId;
use tokio::sync::mpsc;

use crate::collection::CollectionHandle;
use crate::error::Error;
use crate::grep_match::Matcher;
use crate::grep_scan::{GrepSlot, Sub};
use crate::repository::Repository;
use crate::types::Located;

/// Most queries in one grep.
pub const MAX_GREP_QUERIES: usize = 64;
/// Longest pattern, in UTF-8 bytes.
pub const MAX_GREP_PATTERN_BYTES: usize = 4096;
/// Most collections in one grep.
pub const MAX_GREP_COLLECTIONS: usize = 16;
/// Default [`GrepOptions::max_hits`].
pub const DEFAULT_GREP_MAX_HITS: u64 = 10_000;
/// Largest [`GrepOptions::max_hits`].
pub const MAX_GREP_HITS: u64 = 1_000_000;
/// Greps running at once in one repository.
pub const MAX_CONCURRENT_GREPS: usize = 32;

/// Batch messages queued per grep before it is detached as lagging.
const GREP_CHANNEL_CAP: usize = 64;

/// One query. `pattern` is a literal byte string (its UTF-8 bytes), or a regex when `regex`.
///
/// Regexes run in byte mode: Unicode is off, `.` matches any byte except `\n`, classes are ASCII,
/// and non-ASCII characters in a regex are rejected (write `\xHH`). `case_insensitive` folds ASCII
/// letters only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepQuery {
    /// The literal, or the regex source.
    pub pattern: String,
    /// `pattern` is a regex rather than a literal.
    pub regex: bool,
    /// Fold ASCII letters.
    pub case_insensitive: bool,
}

/// Which bytes of a record are searched. A match never spans two of the searched parts. No HTTP
/// transfer or content decoding is applied: bytes are matched as stored in the WARC record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrepScope {
    /// The body of a non-HTTP record; for a record whose WARC `Content-Type` is
    /// `application/http`, the bytes after the first CRLF CRLF of the block.
    Payload,
    /// The WARC head, plus the HTTP head of an `application/http` record.
    Headers,
    /// The whole record bytes.
    #[default]
    All,
}

/// Which events a grep generates. Counters in [`GrepStats`] are maintained regardless, and
/// `max_hits` applies to matches whether or not hits are delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrepEvents {
    /// [`GrepEvent::Hit`].
    pub hits: bool,
    /// [`GrepEvent::Unreadable`].
    pub unreadable: bool,
    /// [`GrepEvent::Progress`].
    pub progress: bool,
}

impl GrepEvents {
    /// Every kind of event.
    pub const ALL: GrepEvents = GrepEvents {
        hits: true,
        unreadable: true,
        progress: true,
    };
    /// No event: only the counters of the final report.
    pub const NONE: GrepEvents = GrepEvents {
        hits: false,
        unreadable: false,
        progress: false,
    };
}

impl Default for GrepEvents {
    fn default() -> Self {
        GrepEvents::ALL
    }
}

/// Options of [`Repository::grep`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepOptions {
    /// `1..=MAX_GREP_QUERIES` queries, OR'ed; each hit reports the set of matching queries.
    pub queries: Vec<GrepQuery>,
    /// What is searched.
    pub scope: GrepScope,
    /// Stop after this many hits, `1..=MAX_GREP_HITS`.
    pub max_hits: u64,
    /// Which events are generated (default all).
    pub events: GrepEvents,
}

impl GrepOptions {
    /// `scope = All`, `max_hits = DEFAULT_GREP_MAX_HITS`.
    #[must_use]
    pub fn new(queries: Vec<GrepQuery>) -> GrepOptions {
        GrepOptions {
            queries,
            scope: GrepScope::All,
            max_hits: DEFAULT_GREP_MAX_HITS,
            events: GrepEvents::ALL,
        }
    }
}

/// Indices of the queries that matched (bit `i` is query `i`; at most 64 queries).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuerySet(u64);

impl QuerySet {
    pub(crate) fn from_bits(bits: u64) -> QuerySet {
        QuerySet(bits)
    }

    /// The raw bit set.
    #[must_use]
    pub fn bits(self) -> u64 {
        self.0
    }

    /// True if query `i` matched (false for `i >= 64`).
    #[must_use]
    pub fn contains(self, i: usize) -> bool {
        u32::try_from(i)
            .ok()
            .and_then(|s| 1u64.checked_shl(s))
            .is_some_and(|bit| self.0 & bit != 0)
    }

    /// True if no query matched.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Number of queries that matched.
    #[must_use]
    pub fn len(self) -> usize {
        usize::try_from(self.0.count_ones()).unwrap_or(usize::MAX)
    }

    /// The matching query indices, ascending.
    pub fn iter(self) -> impl Iterator<Item = usize> {
        (0..64usize).filter(move |&i| self.contains(i))
    }
}

/// One event of a grep, in delivery order per collection; collections interleave.
#[derive(Debug)]
pub enum GrepEvent {
    /// A live record of `[header, T)` matched at least one query.
    Hit {
        /// Where the record lives.
        located: Arc<Located>,
        /// The queries that matched.
        queries: QuerySet,
    },
    /// A record the grep needed could not be read or decoded (digest, frame, parse,
    /// `NotDecodable`). The grep continues.
    Unreadable {
        /// Where the record lives.
        located: Arc<Located>,
        /// Why it could not be read.
        error: Arc<Error>,
    },
    /// Progress of this grep in one collection. `cdx_done / cdx_total` is the completion fraction
    /// (CDX bytes of the grep's range `[first, T)`; proportional to index lines, not to data
    /// volume). The first event of a collection has `cdx_done = 0`; then one is emitted every
    /// 16 384 in-range CDX lines (cumulative counts). No final event: the report carries the
    /// final values.
    Progress {
        /// The collection.
        collection: CollectionId,
        /// Records examined so far.
        records: u64,
        /// Decoded bytes of those records.
        bytes: u64,
        /// CDX bytes of the range consumed so far.
        cdx_done: u64,
        /// CDX bytes of the whole range, `T - first`.
        cdx_total: u64,
    },
}

/// Counters of one collection of one grep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GrepStats {
    /// Records examined (read OK or unreadable) for this grep.
    pub records: u64,
    /// Decoded bytes of those records.
    pub bytes: u64,
    /// Hits counted (a hit reserved in a batch dropped because the grep lagged is included).
    pub hits: u64,
    /// Records that could not be read.
    pub unreadable: u64,
    /// CDX bytes of the grep's range consumed (tombstone lines and unmatched records included).
    /// `complete` implies `cdx_done == cdx_total`.
    pub cdx_done: u64,
    /// `cdx_total = T - first`, 0 if the grep failed before it was positioned.
    pub cdx_total: u64,
    /// True iff every live record of the grep's range was examined (no error, not truncated).
    pub complete: bool,
}

/// The outcome of one collection of a grep.
#[derive(Debug, Clone)]
pub struct CollectionGrep {
    /// The collection.
    pub collection: CollectionId,
    /// Its counters.
    pub stats: GrepStats,
    /// Why this collection's part stopped early (`StaleLocator`, `UnknownCollection`,
    /// `ShuttingDown`, `GrepLagged`, a CDX error, ...); `None` when complete or truncated.
    pub error: Option<Arc<Error>>,
}

/// The final report of a grep.
#[derive(Debug, Clone)]
pub struct GrepReport {
    /// In the order of the request.
    pub collections: Vec<CollectionGrep>,
    /// Hits counted (a hit reserved in a batch dropped because the grep lagged is included).
    pub hits: u64,
    /// `max_hits` was reached.
    pub truncated: bool,
}

/// State shared by a [`Grep`] and its subscriptions (one per collection).
pub(crate) struct GrepShared {
    pub(crate) max_hits: u64,
    /// Hits counted; never exceeds `max_hits`.
    pub(crate) hits: AtomicU64,
    /// Set once `max_hits` refused a hit.
    pub(crate) truncated: AtomicBool,
    /// Set once a `try_send` found the channel full.
    pub(crate) lagged: AtomicBool,
    /// One entry per subscription that ended. A leaf lock: push and take only, never held with
    /// another lock.
    pub(crate) outcomes: std::sync::Mutex<Vec<CollectionGrep>>,
}

impl GrepShared {
    /// Reserves one hit. On refusal sets `truncated` and returns false.
    pub(crate) fn take_hit(&self) -> bool {
        let ok = self
            .hits
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |h| {
                if h < self.max_hits {
                    h.checked_add(1)
                } else {
                    None
                }
            })
            .is_ok();
        if !ok {
            self.truncated.store(true, Ordering::Release);
        }
        ok
    }
}

/// A running grep. `Send`, not `Clone`. Dropping it detaches it from every scanner (at their next
/// batch) and releases its concurrency slot.
pub struct Grep {
    rx: mpsc::Receiver<Vec<GrepEvent>>,
    buf: std::vec::IntoIter<GrepEvent>,
    shared: Arc<GrepShared>,
    order: Vec<CollectionId>,
    report: Option<GrepReport>,
    ended: bool,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Grep {
    /// Next event; `Ready(None)` once every collection has ended (then [`report`](Self::report)
    /// is `Some`).
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<GrepEvent>> {
        loop {
            if let Some(e) = self.buf.next() {
                return Poll::Ready(Some(e));
            }
            if self.ended {
                return Poll::Ready(None);
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(v)) => self.buf = v.into_iter(),
                Poll::Ready(None) => {
                    self.ended = true;
                    self.report = Some(self.build_report());
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    /// The next event, `None` once every collection has ended. Cancel-safe.
    pub async fn next(&mut self) -> Option<GrepEvent> {
        std::future::poll_fn(|cx| self.poll_next(cx)).await
    }

    /// The final report, once, after [`next`](Self::next) returned `None`; `None` before that or
    /// on a second call.
    pub fn report(&mut self) -> Option<GrepReport> {
        self.report.take()
    }

    fn build_report(&self) -> GrepReport {
        let mut outcomes = std::mem::take(
            &mut *self
                .shared
                .outcomes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let collections = self
            .order
            .iter()
            .map(
                |id| match outcomes.iter().position(|o| o.collection == *id) {
                    Some(i) => outcomes.swap_remove(i),
                    None => CollectionGrep {
                        collection: *id,
                        stats: GrepStats::default(),
                        error: Some(Arc::new(Error::Internal(
                            "grep scanner stopped without a result",
                        ))),
                    },
                },
            )
            .collect();
        GrepReport {
            collections,
            hits: self.shared.hits.load(Ordering::Acquire),
            truncated: self.shared.truncated.load(Ordering::Acquire),
        }
    }
}

impl Repository {
    /// Starts a grep over `collections` (handles of this repository). See the module docs for the
    /// coverage guarantee and the circular shared scan.
    ///
    /// # Cancellation
    ///
    /// Grep is read-only: dropping the future, or the returned [`Grep`], leaves nothing behind.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`; `UnknownCollection` (a deleted handle); `InvalidQuery("grep collections")`
    /// (empty, more than `MAX_GREP_COLLECTIONS`, duplicate collection, handle of another
    /// repository); `InvalidQuery("grep queries")` (0 or more than `MAX_GREP_QUERIES`);
    /// `InvalidQuery("grep pattern")` (empty or longer than `MAX_GREP_PATTERN_BYTES`);
    /// `InvalidQuery("grep max_hits")` (0 or more than `MAX_GREP_HITS`); `InvalidPattern`;
    /// `TooManyGreps`; `Join`.
    pub async fn grep(
        &self,
        collections: &[CollectionHandle],
        opts: GrepOptions,
    ) -> Result<Grep, Error> {
        let core = &self.inner.core;
        if core.closed.load(Ordering::Acquire) || core.is_closing() {
            return Err(Error::ShuttingDown);
        }
        let bad_collections = || Error::InvalidQuery("grep collections");
        if collections.is_empty() || collections.len() > MAX_GREP_COLLECTIONS {
            return Err(bad_collections());
        }
        for (i, h) in collections.iter().enumerate() {
            if !Arc::ptr_eq(&h.core, core) {
                return Err(bad_collections());
            }
            // At most 16 handles: a pairwise comparison is cheap.
            if collections
                .iter()
                .skip(i.saturating_add(1))
                .any(|o| o.id() == h.id())
            {
                return Err(bad_collections());
            }
            h.shared.check_live()?;
        }
        if opts.queries.is_empty() || opts.queries.len() > MAX_GREP_QUERIES {
            return Err(Error::InvalidQuery("grep queries"));
        }
        if opts
            .queries
            .iter()
            .any(|q| q.pattern.is_empty() || q.pattern.len() > MAX_GREP_PATTERN_BYTES)
        {
            return Err(Error::InvalidQuery("grep pattern"));
        }
        if opts.max_hits == 0 || opts.max_hits > MAX_GREP_HITS {
            return Err(Error::InvalidQuery("grep max_hits"));
        }
        let permit = Arc::clone(&core.grep_sem)
            .try_acquire_owned()
            .map_err(|_| Error::TooManyGreps)?;
        let GrepOptions {
            queries,
            scope,
            max_hits,
            events,
        } = opts;
        let matcher = Arc::new(
            tokio::task::spawn_blocking(move || Matcher::compile(&queries))
                .await
                .map_err(Error::Join)??,
        );
        let (tx, rx) = mpsc::channel(GREP_CHANNEL_CAP);
        let shared = Arc::new(GrepShared {
            max_hits,
            hits: AtomicU64::new(0),
            truncated: AtomicBool::new(false),
            lagged: AtomicBool::new(false),
            outcomes: std::sync::Mutex::new(Vec::with_capacity(collections.len())),
        });
        for h in collections {
            let sub = Sub::new(
                h.id(),
                tx.clone(),
                Arc::clone(&shared),
                Arc::clone(&matcher),
                scope,
                events,
            );
            // On error `rx` drops with `?`, which detaches the subscriptions already attached.
            GrepSlot::attach(core, &h.shared, sub)?;
        }
        drop(tx);
        Ok(Grep {
            rx,
            buf: Vec::new().into_iter(),
            shared,
            order: collections.iter().map(CollectionHandle::id).collect(),
            report: None,
            ended: false,
            _permit: permit,
        })
    }
}
