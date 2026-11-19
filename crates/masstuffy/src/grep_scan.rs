//! The shared circular scanner of grep: one task per (collection, generation) with live
//! subscriptions. See the `grep` module docs for the guarantees.
//!
//! Locks: `GrepSlot::state` is a leaf (it may take the `Published::snapshot` read lock inside,
//! in `attach`, and nothing else). `GrepShared::outcomes` is a leaf taken alone. Neither is ever
//! held across an `.await`, and subscriptions are always finished after the slot lock is released.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use masstuffy_collection::{CdxLines, CollectionId, Kind, ReadCtx, ReadError, Snapshot};
use regex_automata::PatternSet;
use regex_automata::meta::Cache;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;

use crate::collection::Shared;
use crate::error::Error;
use crate::grep::{CollectionGrep, GrepEvent, GrepEvents, GrepScope, GrepShared, GrepStats};
use crate::grep_match::{Matcher, Parts};
use crate::read::retry_snapshot;
use crate::repository::Core;
use crate::types::Located;
use crate::writer::Published;

/// CDX lines per batch (one blocking job, one read permit).
const GREP_BATCH_LINES: usize = 256;
/// Decoded bytes per batch.
const GREP_BATCH_BYTES: u64 = 16 << 20;
/// In-range CDX lines between two `Progress` events of one subscription.
const PROGRESS_LINES: u64 = 16_384;

/// Test hooks of the scanner.
#[cfg(test)]
pub(crate) struct GrepHooks {
    /// When `Some`, the scanner acquires and forgets one permit at the start of each iteration.
    pub(crate) gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// CDX lines per batch; 0 is the default.
    pub(crate) batch_lines: std::sync::atomic::AtomicUsize,
    /// Batches completed.
    pub(crate) batches: tokio::sync::watch::Sender<u64>,
    /// Wraps of the circular scan.
    pub(crate) passes: std::sync::atomic::AtomicU64,
    /// In-range lines between progress events; 0 is the default.
    pub(crate) progress_lines: std::sync::atomic::AtomicU64,
    /// Scanner tasks spawned.
    pub(crate) started: std::sync::atomic::AtomicU64,
    /// Scanner tasks that returned.
    pub(crate) exited: tokio::sync::watch::Sender<u64>,
    /// Successful record reads by `(segment, offset)`.
    pub(crate) reads: Mutex<std::collections::HashMap<(u32, u64), u32>>,
}

#[cfg(test)]
impl Default for GrepHooks {
    fn default() -> Self {
        GrepHooks {
            gate: Mutex::new(None),
            batch_lines: std::sync::atomic::AtomicUsize::new(0),
            batches: tokio::sync::watch::channel(0).0,
            passes: std::sync::atomic::AtomicU64::new(0),
            progress_lines: std::sync::atomic::AtomicU64::new(0),
            started: std::sync::atomic::AtomicU64::new(0),
            exited: tokio::sync::watch::channel(0).0,
            reads: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

/// How a subscription ended.
pub(crate) enum SubEnd {
    Done,
    Truncated,
    Detached,
    Failed(Arc<Error>),
}

/// One (grep, collection) subscription. Owned exclusively by the scanner task: moved into and out
/// of the blocking batch job, never shared.
pub(crate) struct Sub {
    pub(crate) collection: CollectionId,
    /// Its own clone of the grep's sender; dropped when the subscription ends.
    pub(crate) tx: mpsc::Sender<Vec<GrepEvent>>,
    pub(crate) shared: Arc<GrepShared>,
    pub(crate) matcher: Arc<Matcher>,
    pub(crate) cache: Cache,
    pub(crate) set: PatternSet,
    pub(crate) scope: GrepScope,
    /// `T`: the fsynced CDX length at attach (a line boundary).
    pub(crate) target: u64,
    /// Scanner position when the subscription was picked up (a line start).
    pub(crate) start: u64,
    pub(crate) wrapped: bool,
    pub(crate) stats: GrepStats,
    pub(crate) since_progress: u64,
    pub(crate) events: GrepEvents,
    /// Events of the current batch.
    pub(crate) out: Vec<GrepEvent>,
    pub(crate) end: Option<SubEnd>,
}

impl Sub {
    pub(crate) fn new(
        collection: CollectionId,
        tx: mpsc::Sender<Vec<GrepEvent>>,
        shared: Arc<GrepShared>,
        matcher: Arc<Matcher>,
        scope: GrepScope,
        events: GrepEvents,
    ) -> Sub {
        let cache = matcher.create_cache();
        let set = matcher.pattern_set();
        Sub {
            collection,
            tx,
            shared,
            matcher,
            cache,
            set,
            scope,
            target: 0,
            start: 0,
            wrapped: false,
            stats: GrepStats::default(),
            since_progress: 0,
            events,
            out: Vec::new(),
            end: None,
        }
    }

    /// End of the post-wrap range: `[first, wrap_end)`.
    fn wrap_end(&self) -> u64 {
        self.start.min(self.target)
    }

    fn active(&self) -> bool {
        self.end.is_none()
    }

    /// True if the line at `at` is in this subscription's remaining range.
    fn needs(&self, at: u64) -> bool {
        self.active()
            && if self.wrapped {
                at < self.wrap_end()
            } else {
                at >= self.start && at < self.target
            }
    }

    /// True if everything this subscription needs has been swept when the scanner is at `p`.
    fn done_at(&self, p: u64, first: u64) -> bool {
        if self.wrapped {
            p >= self.wrap_end()
        } else {
            p >= self.target && self.wrap_end() <= first
        }
    }

    /// Position up to which the sweep must go for this subscription.
    fn sweep_end(&self) -> u64 {
        if self.wrapped {
            self.wrap_end()
        } else {
            self.target
        }
    }
}

/// Ends a subscription: records its outcome (unless detached) and drops it (and its sender).
/// Called with no lock held. The outcome is pushed before `tx` is dropped, so a consumer that
/// sees the channel closed always finds every outcome.
pub(crate) fn finish(sub: Sub, end: SubEnd) {
    let (complete, error) = match end {
        SubEnd::Done => (true, None),
        SubEnd::Truncated => (false, None),
        SubEnd::Failed(e) => (false, Some(e)),
        SubEnd::Detached => return,
    };
    let Sub {
        collection,
        tx,
        shared,
        mut stats,
        ..
    } = sub;
    debug_assert!(
        !complete || stats.cdx_done == stats.cdx_total,
        "complete grep with a CDX remainder"
    );
    stats.complete = complete;
    shared
        .outcomes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(CollectionGrep {
            collection,
            stats,
            error,
        });
    drop(tx);
}

struct Link {
    id: u64,
    generation: u32,
    pending: Vec<Sub>,
}

struct SlotState {
    closed: bool,
    next_id: u64,
    current: Option<Link>,
    tasks: Vec<JoinHandle<()>>,
}

/// The scanner slot of one collection.
///
/// Invariant: if `current` is `Some(link)`, the scanner task with `link.id` is alive. It removes
/// its link, under the same lock, in the same critical section where it decides to exit; a
/// [`ScannerGuard`] removes it and fails its pending subscriptions on any other exit, panics
/// included. So a subscription pushed into `pending` is always picked up or failed, never lost.
pub(crate) struct GrepSlot {
    state: Mutex<SlotState>,
    #[cfg(test)]
    pub(crate) hooks: Arc<GrepHooks>,
}

impl GrepSlot {
    pub(crate) fn new() -> GrepSlot {
        GrepSlot {
            state: Mutex::new(SlotState {
                closed: false,
                next_id: 0,
                current: None,
                tasks: Vec::new(),
            }),
            #[cfg(test)]
            hooks: Arc::new(GrepHooks::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, SlotState> {
        // A poisoned lock is recovered: the guarded state is plain data updated in short
        // critical sections.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Attaches `sub` to the collection's scanner, spawning one if needed. Never awaits. A
    /// subscription of an older generation is failed with `StaleLocator`.
    ///
    /// # Errors
    ///
    /// `ShuttingDown` once the slot is closed.
    pub(crate) fn attach(
        core: &Arc<Core>,
        shared: &Arc<Shared>,
        mut sub: Sub,
    ) -> Result<(), Error> {
        let slot = &shared.grep;
        let mut stale: Vec<Sub> = Vec::new();
        let mut stale_gen = 0;
        let snap_gen;
        {
            let mut st = slot.lock();
            if st.closed {
                return Err(Error::ShuttingDown);
            }
            // Takes the `Published::snapshot` read lock under the slot lock (see the lock order).
            let (snap, end) = shared.published.tail();
            snap_gen = snap.generation();
            sub.target = end;
            match st.current.as_mut() {
                Some(link) if link.generation == snap_gen => link.pending.push(sub),
                _ => {
                    if let Some(old) = st.current.take() {
                        stale_gen = old.generation;
                        stale = old.pending;
                    }
                    let id = st.next_id;
                    st.next_id = st.next_id.wrapping_add(1);
                    st.current = Some(Link {
                        id,
                        generation: snap_gen,
                        pending: vec![sub],
                    });
                    st.tasks.retain(|h| !h.is_finished());
                    #[cfg(test)]
                    slot.hooks.started.fetch_add(1, Ordering::AcqRel);
                    st.tasks.push(tokio::spawn(scanner(
                        Arc::clone(core),
                        Arc::clone(shared),
                        id,
                        snap,
                    )));
                }
            }
        }
        for s in stale {
            let e = Error::StaleLocator {
                expected: snap_gen,
                got: stale_gen,
            };
            finish(s, SubEnd::Failed(Arc::new(e)));
        }
        Ok(())
    }

    /// Shutdown: refuses new attaches and returns the task handles (joined by the caller).
    pub(crate) fn close(&self) -> Vec<JoinHandle<()>> {
        let mut st = self.lock();
        st.closed = true;
        std::mem::take(&mut st.tasks)
    }

    /// If the current link is `id`'s, removes it and returns its pending subscriptions.
    fn take_link(&self, id: u64) -> Vec<Sub> {
        let mut st = self.lock();
        match st.current.take() {
            Some(l) if l.id == id => l.pending,
            other => {
                st.current = other;
                Vec::new()
            }
        }
    }
}

/// Fails the link's pending subscriptions if the scanner exits without having done so (any other
/// exit, panics included), and counts the exit in tests.
struct ScannerGuard {
    shared: Arc<Shared>,
    id: u64,
    armed: bool,
}

impl ScannerGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ScannerGuard {
    fn drop(&mut self) {
        if self.armed {
            let err = Arc::new(Error::Internal("grep scanner stopped"));
            for s in self.shared.grep.take_link(self.id) {
                finish(s, SubEnd::Failed(Arc::clone(&err)));
            }
        }
        #[cfg(test)]
        self.shared
            .grep
            .hooks
            .exited
            .send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// Terminal path: removes our link, then fails every local and pending subscription with `err`.
fn fail_all(shared: &Shared, id: u64, mut subs: Vec<Sub>, err: &Arc<Error>) {
    subs.extend(shared.grep.take_link(id));
    for s in subs {
        finish(s, SubEnd::Failed(Arc::clone(err)));
    }
}

struct ScanState {
    snap: Arc<Snapshot>,
    generation: u32,
    /// Offset of the first line after the CDX header.
    first: u64,
    /// Next line to read (a line start); starts at `first`.
    p: u64,
    lines: Option<CdxLines>,
    lines_end: u64,
    /// Sorted, deduplicated `(segment, offset)` of the `T` lines of `[first, tomb_end)`.
    tombs: Vec<(u32, u64)>,
    tomb_end: u64,
    /// Decoded record buffer, reused.
    buf: Vec<u8>,
}

/// The scanner task of one (collection, generation).
async fn scanner(core: Arc<Core>, shared: Arc<Shared>, id: u64, snap: Arc<Snapshot>) {
    let mut guard = ScannerGuard {
        shared: Arc::clone(&shared),
        id,
        armed: true,
    };
    scan_loop(&core, &shared, id, snap, &mut guard).await;
}

/// The error that ends a scanner, if any: shutdown, deletion or a generation change.
fn terminal_error(core: &Core, shared: &Shared, generation: u32) -> Option<Error> {
    if core.is_closing() {
        return Some(Error::ShuttingDown);
    }
    if shared.is_deleted() {
        return Some(Error::UnknownCollection(shared.id.to_string()));
    }
    let cur = shared.published.load().generation();
    (cur != generation).then_some(Error::StaleLocator {
        expected: cur,
        got: generation,
    })
}

async fn scan_loop(
    core: &Arc<Core>,
    shared: &Arc<Shared>,
    id: u64,
    snap: Arc<Snapshot>,
    guard: &mut ScannerGuard,
) {
    let generation = snap.generation();
    let slot = &shared.grep;

    // The offset of the first line, in a blocking job.
    let Ok(permit) = Arc::clone(&core.read_sem).acquire_owned().await else {
        fail_all(shared, id, Vec::new(), &Arc::new(Error::ShuttingDown));
        guard.disarm();
        return;
    };
    let first_job = {
        let snap = Arc::clone(&snap);
        let published = Arc::clone(&shared.published);
        let collection = shared.id;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let (cur, end) = published.tail();
            if cur.generation() != snap.generation() {
                return Err(Error::StaleLocator {
                    expected: cur.generation(),
                    got: snap.generation(),
                });
            }
            snap.cdx_lines(None, end)
                .map(|l| l.position())
                .map_err(|source| Error::Collection { collection, source })
        })
        .await
    };
    let first = match first_job {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => {
            fail_all(shared, id, Vec::new(), &Arc::new(e));
            guard.disarm();
            return;
        }
        // A panic: the guard fails the pending subscriptions.
        Err(e) => {
            tracing::warn!("grep scanner start panicked: {e}");
            return;
        }
    };

    let mut st = ScanState {
        snap,
        generation,
        first,
        p: first,
        lines: None,
        lines_end: first,
        tombs: Vec::new(),
        tomb_end: first,
        buf: Vec::new(),
    };
    let mut subs: Vec<Sub> = Vec::new();

    loop {
        // 1. Test gate.
        #[cfg(test)]
        {
            let gate = slot
                .hooks
                .gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(sem) = gate
                && let crate::rebuild::Either::Left(Ok(p)) =
                    crate::rebuild::race(sem.acquire(), core.closing_signal()).await
            {
                p.forget();
            }
        }

        // 2. Pick up joiners, or exit: the decision and the link removal are one critical
        // section.
        enum Slot {
            Go,
            Exit,
            Orphan,
        }
        let decision = {
            let mut g = slot.lock();
            match g.current.as_mut() {
                Some(link) if link.id == id => {
                    for mut s in link.pending.drain(..) {
                        s.start = st.p;
                        s.wrapped = false;
                        s.stats.cdx_total = s.target.saturating_sub(st.first);
                        if s.events.progress {
                            s.out.push(GrepEvent::Progress {
                                collection: s.collection,
                                records: 0,
                                bytes: 0,
                                cdx_done: 0,
                                cdx_total: s.stats.cdx_total,
                            });
                        }
                        subs.push(s);
                    }
                    if subs.is_empty() {
                        g.current = None;
                        Slot::Exit
                    } else {
                        Slot::Go
                    }
                }
                _ => Slot::Orphan,
            }
        };
        match decision {
            Slot::Go => {}
            Slot::Exit => {
                guard.disarm();
                return;
            }
            Slot::Orphan => {
                let cur = shared.published.load().generation();
                let e = Error::StaleLocator {
                    expected: cur,
                    got: generation,
                };
                fail_all(shared, id, subs, &Arc::new(e));
                guard.disarm();
                return;
            }
        }

        // 3. Terminal checks.
        if let Some(e) = terminal_error(core, shared, generation) {
            fail_all(shared, id, subs, &Arc::new(e));
            guard.disarm();
            return;
        }

        // 4. Prune detached, lagging and truncated subscriptions.
        let mut i = 0;
        while i < subs.len() {
            let end = match subs.get(i) {
                Some(s) if s.tx.is_closed() => Some(SubEnd::Detached),
                Some(s) if s.shared.lagged.load(Ordering::Acquire) => {
                    Some(SubEnd::Failed(Arc::new(Error::GrepLagged)))
                }
                Some(s) if s.shared.truncated.load(Ordering::Acquire) => Some(SubEnd::Truncated),
                _ => None,
            };
            match end {
                Some(e) => finish(subs.swap_remove(i), e),
                None => i += 1,
            }
        }
        if subs.is_empty() {
            continue;
        }

        // 5. One read permit per batch.
        let Ok(permit) = Arc::clone(&core.read_sem).acquire_owned().await else {
            fail_all(shared, id, subs, &Arc::new(Error::ShuttingDown));
            guard.disarm();
            return;
        };

        // 6. The batch, in a blocking job.
        let job = {
            let core = Arc::clone(core);
            let shared = Arc::clone(shared);
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let mut ctx = core.read_ctxs.take();
                let mut st = st;
                let mut subs = subs;
                let res = run_batch(
                    &mut st,
                    &mut subs,
                    &shared.published,
                    &mut ctx,
                    shared.id,
                    #[cfg(test)]
                    &shared.grep.hooks,
                );
                core.read_ctxs.give(ctx);
                (st, subs, res)
            })
            .await
        };
        let (st2, mut subs2, res) = match job {
            Ok(t) => t,
            // A panic: state and subscriptions are lost; the guard fails the slot's pending ones
            // and the report falls back for the rest.
            Err(e) => {
                tracing::warn!("grep batch panicked: {e}");
                return;
            }
        };
        st = st2;

        // 7. A failed batch ends every subscription.
        if let Err(e) = res {
            // A rebuild that finished during the batch removed the old CDX and segments: report
            // it as `StaleLocator`, not as the I/O error the reopen produced.
            let cur = shared.published.load().generation();
            let e = if shared.is_deleted() {
                Error::UnknownCollection(shared.id.to_string())
            } else if cur != generation {
                Error::StaleLocator {
                    expected: cur,
                    got: generation,
                }
            } else {
                e
            };
            fail_all(shared, id, subs2, &Arc::new(e));
            guard.disarm();
            return;
        }

        // 8. Deliver, never waiting for a consumer.
        for s in &mut subs2 {
            if s.out.is_empty() {
                continue;
            }
            match s.tx.try_send(std::mem::take(&mut s.out)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    s.shared.lagged.store(true, Ordering::Release);
                    s.end = Some(SubEnd::Failed(Arc::new(Error::GrepLagged)));
                }
                Err(TrySendError::Closed(_)) => s.end = Some(SubEnd::Detached),
            }
        }
        let mut live = Vec::with_capacity(subs2.len());
        for mut s in subs2 {
            match s.end.take() {
                Some(e) => finish(s, e),
                None => live.push(s),
            }
        }
        subs = live;

        // 9. Test counter.
        #[cfg(test)]
        slot.hooks.batches.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// `Some(x)` unless `x` is the offset of the first line (the reader's "after the header").
fn from(x: u64, first: u64) -> Option<u64> {
    (x != first).then_some(x)
}

fn add(a: u64, b: u64) -> u64 {
    a.saturating_add(b)
}

/// Marks `Done` every active subscription whose range is fully swept at `p`.
fn mark_done(subs: &mut [Sub], p: u64, first: u64) {
    for s in subs.iter_mut().filter(|s| s.active()) {
        if s.done_at(p, first) {
            s.end = Some(SubEnd::Done);
        }
    }
}

fn sweep_end(subs: &[Sub]) -> u64 {
    subs.iter()
        .filter(|s| s.active())
        .map(Sub::sweep_end)
        .max()
        .unwrap_or(0)
}

/// One batch of the scan (blocking): extends the tombstones to the largest target, then reads
/// CDX lines from the current position, wrapping once the end of the sweep is reached, until the
/// batch limit or until no subscription is active.
fn run_batch(
    st: &mut ScanState,
    subs: &mut [Sub],
    published: &Published,
    ctx: &mut ReadCtx,
    collection: CollectionId,
    #[cfg(test)] hooks: &GrepHooks,
) -> Result<(), Error> {
    let ScanState {
        snap,
        generation,
        first,
        p,
        lines,
        lines_end,
        tombs,
        tomb_end,
        buf,
    } = st;
    let first = *first;
    let generation = *generation;
    let cdx_err = |source| Error::Collection { collection, source };

    #[cfg(test)]
    let batch_lines = match hooks.batch_lines.load(Ordering::Acquire) {
        0 => GREP_BATCH_LINES,
        n => n,
    };
    #[cfg(not(test))]
    let batch_lines = GREP_BATCH_LINES;
    #[cfg(test)]
    let progress_lines = match hooks.progress_lines.load(Ordering::Acquire) {
        0 => PROGRESS_LINES,
        n => n,
    };
    #[cfg(not(test))]
    let progress_lines = PROGRESS_LINES;

    // Tombstones of every line below the largest target (a superset for the smaller targets).
    let max_t = subs
        .iter()
        .filter(|s| s.active())
        .map(|s| s.target)
        .max()
        .unwrap_or(0);
    if max_t > *tomb_end {
        let mut l = snap
            .cdx_lines(from(*tomb_end, first), max_t)
            .map_err(cdx_err)?;
        while let Some(line) = l.next_line().map_err(cdx_err)? {
            if line.entry.kind == Kind::Tombstone {
                tombs.push((line.entry.segment, line.entry.offset));
            }
        }
        tombs.sort_unstable();
        tombs.dedup();
        *tomb_end = max_t;
    }

    let mut bound = sweep_end(subs);
    let mut lines_n = 0usize;
    let mut bytes = 0u64;
    loop {
        if *p >= bound {
            mark_done(subs, *p, first);
            if !subs.iter().any(Sub::active) {
                break;
            }
            // Every remaining subscription is unwrapped with `p >= target` and a range below its
            // start to cover: wrap once.
            *p = first;
            *lines = None;
            for s in subs.iter_mut().filter(|s| s.active()) {
                s.wrapped = true;
            }
            #[cfg(test)]
            hooks.passes.fetch_add(1, Ordering::AcqRel);
            bound = sweep_end(subs);
            continue;
        }
        if lines_n >= batch_lines || bytes >= GREP_BATCH_BYTES {
            break;
        }
        if lines.is_none() || *lines_end < bound {
            *lines = Some(snap.cdx_lines(from(*p, first), bound).map_err(cdx_err)?);
            *lines_end = bound;
        }
        let Some(reader) = lines.as_mut() else {
            return Err(Error::Internal("grep cdx reader"));
        };
        let Some(line) = reader.next_line().map_err(cdx_err)? else {
            *p = *lines_end;
            continue;
        };
        lines_n = lines_n.saturating_add(1);
        let at = line.at;
        let next = line.next;
        let len = next
            .checked_sub(at)
            .ok_or(Error::Internal("cdx line offsets"))?;
        // Every in-range line counts for the progress, whatever its kind.
        let mut needed = false;
        for sub in subs.iter_mut().filter(|s| s.needs(at)) {
            needed = true;
            sub.stats.cdx_done = add(sub.stats.cdx_done, len);
            sub.since_progress = add(sub.since_progress, 1);
            if sub.events.progress && sub.since_progress >= progress_lines {
                sub.since_progress = 0;
                sub.out.push(GrepEvent::Progress {
                    collection: sub.collection,
                    records: sub.stats.records,
                    bytes: sub.stats.bytes,
                    cdx_done: sub.stats.cdx_done,
                    cdx_total: sub.stats.cdx_total,
                });
            }
        }
        let wanted = needed
            && line.entry.kind == Kind::Add
            && tombs
                .binary_search(&(line.entry.segment, line.entry.offset))
                .is_err();
        if wanted {
            let mut located: Option<Arc<Located>> = None;
            for attempt in 0..2 {
                match snap.read(&line.entry, ctx, buf) {
                    Ok(rec) => {
                        let len = u64::try_from(rec.as_bytes().len()).unwrap_or(u64::MAX);
                        bytes = add(bytes, len);
                        #[cfg(test)]
                        {
                            let mut r = hooks.reads.lock().unwrap_or_else(PoisonError::into_inner);
                            let n = r
                                .entry((line.entry.segment, line.entry.offset))
                                .or_insert(0);
                            *n = n.saturating_add(1);
                        }
                        let parts = Parts::of(&rec);
                        for sub in subs.iter_mut().filter(|s| s.needs(at)) {
                            sub.stats.records = add(sub.stats.records, 1);
                            sub.stats.bytes = add(sub.stats.bytes, len);
                            let q = sub.matcher.matches(
                                &parts.for_scope(sub.scope),
                                &mut sub.cache,
                                &mut sub.set,
                            );
                            if !q.is_empty() {
                                if sub.shared.take_hit() {
                                    sub.stats.hits = add(sub.stats.hits, 1);
                                    if sub.events.hits {
                                        let l = located.get_or_insert_with(|| {
                                            Arc::new(Located {
                                                collection,
                                                generation,
                                                entry: line.entry.clone().into_owned(),
                                            })
                                        });
                                        sub.out.push(GrepEvent::Hit {
                                            located: Arc::clone(l),
                                            queries: q,
                                        });
                                    }
                                } else {
                                    sub.end = Some(SubEnd::Truncated);
                                }
                            }
                        }
                        break;
                    }
                    Err(e) => {
                        if attempt == 0
                            && matches!(e, ReadError::SegmentMissing { .. })
                            && let Some(cur) = retry_snapshot(snap, published)?
                        {
                            *snap = cur;
                            continue;
                        }
                        let wants = subs.iter().any(|s| s.needs(at) && s.events.unreadable);
                        let reported = wants.then(|| {
                            let error = Arc::new(match e {
                                ReadError::EntryMismatch(_) | ReadError::Record(_) => {
                                    Error::NotDecodable { source: e }
                                }
                                source => Error::Read { collection, source },
                            });
                            let l = located.get_or_insert_with(|| {
                                Arc::new(Located {
                                    collection,
                                    generation,
                                    entry: line.entry.clone().into_owned(),
                                })
                            });
                            (Arc::clone(l), error)
                        });
                        for sub in subs.iter_mut().filter(|s| s.needs(at)) {
                            sub.stats.records = add(sub.stats.records, 1);
                            sub.stats.unreadable = add(sub.stats.unreadable, 1);
                            if let (true, Some((l, error))) = (sub.events.unreadable, &reported) {
                                sub.out.push(GrepEvent::Unreadable {
                                    located: Arc::clone(l),
                                    error: Arc::clone(error),
                                });
                            }
                        }
                        break;
                    }
                }
            }
        }
        *p = next;
    }
    mark_done(subs, *p, first);
    Ok(())
}
