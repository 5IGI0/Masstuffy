//! The writer task of one collection: group commit, publication, reopen, checkpoints.
//!
//! One tokio task per collection owns the collection's `Writer` (no lock). Callers prepare their
//! records first and queue a [`Request`]; the task drains what is queued (without waiting), makes
//! the whole group durable with one `Writer::commit` (one data fsync, one CDX fsync) inside
//! `spawn_blocking`, then answers every request.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use masstuffy_cdx::Entry;
use masstuffy_collection::{
    Collection, CollectionId, CommitError, Options, Prepared, RecoveryOptions, Snapshot, Writer,
};
use masstuffy_durable::DirLock;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};
use tracing::{debug, error, warn};

use crate::indexer::{IndexFeed, IndexLink};
use crate::resolver::StoreResolver;

/// What a committed request gets back.
#[derive(Debug)]
pub(crate) struct Committed {
    pub(crate) generation: u32,
    pub(crate) entries: Vec<Entry<'static>>,
}

/// Cloneable error delivered to every request of a failed group.
#[derive(Debug, Clone)]
pub(crate) enum WriteError {
    Commit(Arc<CommitError>),
    Reopen(Arc<masstuffy_collection::Error>),
    Internal(&'static str),
}

pub(crate) type Reply = Result<Committed, WriteError>;

/// One caller's prepared records.
pub(crate) struct Request {
    pub(crate) items: Vec<Prepared>,
    /// Sum of the items' stored lengths (saturating).
    pub(crate) bytes: u64,
    pub(crate) reply: oneshot::Sender<Reply>,
    /// Inflight-bytes admission permit; dropped by the writer task after the request is answered,
    /// so a caller that drops its future does not release the memory budget early.
    pub(crate) permit: OwnedSemaphorePermit,
}

pub(crate) enum Msg {
    Commit(Request),
    /// Finish everything queued before this message, write a final checkpoint, reply, exit.
    Shutdown(oneshot::Sender<()>),
}

/// What readers and `stats()` see of a collection. It holds no `Sender`, so the writer task
/// holding it does not keep its own channel open.
pub(crate) struct Published {
    snapshot: RwLock<Arc<Snapshot>>,
    pub(crate) commits: AtomicU64,
    pub(crate) records: AtomicU64,
    pub(crate) cdx_len: AtomicU64,
}

impl Published {
    pub(crate) fn new(snapshot: Arc<Snapshot>, cdx_len: u64) -> Published {
        Published {
            snapshot: RwLock::new(snapshot),
            commits: AtomicU64::new(0),
            records: AtomicU64::new(0),
            cdx_len: AtomicU64::new(cdx_len),
        }
    }

    /// The current snapshot: one read lock and an `Arc` clone. A poisoned lock is recovered: the
    /// guarded value is a plain `Arc`, always consistent.
    pub(crate) fn load(&self) -> Arc<Snapshot> {
        let g = self.snapshot.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&g)
    }

    /// The current snapshot and the fsynced CDX length, read together: the length is loaded while
    /// the snapshot read lock is held. M20 must store a new-generation snapshot and its length
    /// under the write lock (`store_with_len`, added by M20).
    pub(crate) fn tail(&self) -> (Arc<Snapshot>, u64) {
        let g = self.snapshot.read().unwrap_or_else(PoisonError::into_inner);
        let len = self.cdx_len.load(Ordering::Acquire);
        (Arc::clone(&g), len)
    }

    pub(crate) fn store(&self, s: Arc<Snapshot>) {
        let mut g = self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        *g = s;
    }
}

/// Hooks that let tests hold the writer task back or make it lose its writer.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHooks {
    /// When `Some`, the task acquires and forgets one permit before draining each group.
    pub(crate) hold: std::sync::Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Consumed by the next commit: the writer is dropped first so the reopen path runs.
    pub(crate) drop_writer_next: std::sync::atomic::AtomicBool,
    /// One entry is consumed by each `Writer::commit` attempt (before the real commit, which
    /// is then skipped): the injected failure.
    pub(crate) panic_on_exit: std::sync::atomic::AtomicBool,
    pub(crate) script: std::sync::Mutex<std::collections::VecDeque<Fault>>,
}

/// An injected commit failure; nothing is written.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Fault {
    /// A failure that poisons the writer (the group is retried after a reopen).
    Poison,
    /// A failure that does not poison it (the requests are committed one by one).
    Reject,
}

#[cfg(test)]
type Faults = Arc<TestHooks>;
#[cfg(not(test))]
type Faults = ();

/// The injected failure for the next commit attempt, if any (always `None` outside tests).
fn injected(faults: &Faults) -> Option<CommitError> {
    #[cfg(test)]
    {
        let f = faults
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()?;
        Some(match f {
            Fault::Poison => CommitError::Poisoned,
            Fault::Reject => CommitError::WrongGeneration {
                index: 0,
                expected: 0,
                got: 0,
            },
        })
    }
    #[cfg(not(test))]
    {
        let _ = faults;
        None
    }
}

#[cfg(test)]
impl TestHooks {
    async fn before_drain(&self) {
        let sem = self
            .hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(s) = sem
            && let Ok(p) = s.acquire().await
        {
            p.forget();
        }
    }
}

/// Everything the task needs, built by the repository.
pub(crate) struct WriterParams {
    pub(crate) rx: mpsc::Receiver<Msg>,
    pub(crate) writer: Writer,
    pub(crate) id: CollectionId,
    pub(crate) dir: PathBuf,
    pub(crate) opts: Options,
    pub(crate) resolver: Arc<StoreResolver>,
    pub(crate) published: Arc<Published>,
    pub(crate) group_bytes_cap: u64,
    pub(crate) group_requests_cap: usize,
    pub(crate) checkpoint_every: u64,
    pub(crate) lock: Arc<DirLock>,
    pub(crate) feed: Arc<IndexFeed>,
    pub(crate) link: IndexLink,
    #[cfg(test)]
    pub(crate) hooks: Arc<TestHooks>,
}

pub(crate) struct WriterTask {
    rx: mpsc::Receiver<Msg>,
    /// `None`: lost (poisoned and reopen failed, or a blocking task panicked); the next group
    /// reopens.
    writer: Option<Writer>,
    id: CollectionId,
    dir: PathBuf,
    opts: Options,
    resolver: Arc<StoreResolver>,
    published: Arc<Published>,
    group_bytes_cap: u64,
    group_requests_cap: usize,
    checkpoint_every: u64,
    last_checkpoint_len: u64,
    /// Keeps the repository locked until this task has exited.
    _lock: Arc<DirLock>,
    /// What the secondary index tails, and how to wake it.
    feed: Arc<IndexFeed>,
    link: IndexLink,
    #[cfg(test)]
    hooks: Arc<TestHooks>,
}

/// One successful `Writer::commit`.
struct CommitDone {
    generation: u32,
    cdx_len: u64,
    entries: Vec<Entry<'static>>,
    /// The requests (indexes into the group) it covered.
    requests: Range<usize>,
}

/// Where and how to reopen the collection.
struct Target {
    dir: PathBuf,
    opts: Options,
    resolver: Arc<StoreResolver>,
}

struct BlockingOutcome {
    writer: Option<Writer>,
    new_snapshot: Option<Arc<Snapshot>>,
    done: Vec<CommitDone>,
    failed: Vec<(Range<usize>, WriteError)>,
}

impl WriterTask {
    pub(crate) fn new(p: WriterParams) -> WriterTask {
        let last_checkpoint_len = p.writer.cdx_len();
        WriterTask {
            rx: p.rx,
            writer: Some(p.writer),
            id: p.id,
            dir: p.dir,
            opts: p.opts,
            resolver: p.resolver,
            published: p.published,
            group_bytes_cap: p.group_bytes_cap,
            group_requests_cap: p.group_requests_cap,
            checkpoint_every: p.checkpoint_every,
            last_checkpoint_len,
            _lock: p.lock,
            feed: p.feed,
            link: p.link,
            #[cfg(test)]
            hooks: p.hooks,
        }
    }

    /// The task body. Every request read from the queue is committed or answered; nothing is read
    /// past a `Shutdown` message. Exits on `Shutdown` or when every `Sender` is gone.
    pub(crate) async fn run(mut self) {
        let mut pending: Option<Request> = None;
        // Some(ack) once Shutdown was seen (None: the channel closed).
        let mut stop: Option<Option<oneshot::Sender<()>>> = None;
        loop {
            let first = match pending.take() {
                Some(r) => r,
                None if stop.is_some() => break,
                None => match self.rx.recv().await {
                    Some(Msg::Commit(r)) => r,
                    Some(Msg::Shutdown(ack)) => {
                        stop = Some(Some(ack));
                        break;
                    }
                    None => {
                        stop = Some(None);
                        break;
                    }
                },
            };
            #[cfg(test)]
            self.hooks.before_drain().await;
            let mut bytes = first.bytes;
            let mut group = vec![first];
            while stop.is_none() && group.len() < self.group_requests_cap {
                match self.rx.try_recv() {
                    Ok(Msg::Commit(r)) => match bytes.checked_add(r.bytes) {
                        Some(b) if b <= self.group_bytes_cap => {
                            bytes = b;
                            group.push(r);
                        }
                        _ => {
                            pending = Some(r);
                            break;
                        }
                    },
                    Ok(Msg::Shutdown(ack)) => stop = Some(Some(ack)),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => stop = Some(None),
                }
            }
            self.commit_group(group).await;
        }
        self.final_checkpoint().await;
        if let Some(Some(ack)) = stop {
            let _ = ack.send(());
        }
        #[cfg(test)]
        if self.hooks.panic_on_exit.load(Ordering::Acquire) {
            panic!("injected writer task panic");
        }
    }

    async fn commit_group(&mut self, group: Vec<Request>) {
        let n = group.len();
        let total: usize = group.iter().map(|r| r.items.len()).sum();
        let mut items = Vec::with_capacity(total);
        let mut lens = Vec::with_capacity(n);
        let mut slots = Vec::with_capacity(n);
        for r in group {
            let Request {
                items: it,
                bytes: _,
                reply,
                permit,
            } = r;
            lens.push(it.len());
            items.extend(it);
            slots.push((reply, permit));
        }
        #[cfg(test)]
        let drop_first = self
            .hooks
            .drop_writer_next
            .swap(false, std::sync::atomic::Ordering::AcqRel);
        #[cfg(not(test))]
        let drop_first = false;

        let writer = self.writer.take();
        let target = Target {
            dir: self.dir.clone(),
            opts: self.opts,
            resolver: Arc::clone(&self.resolver),
        };
        let job_lens = lens.clone();
        #[cfg(test)]
        let faults = Arc::clone(&self.hooks);
        #[cfg(not(test))]
        let faults = ();
        let joined = tokio::task::spawn_blocking(move || {
            commit_blocking(writer, items, &job_lens, &target, drop_first, &faults)
        })
        .await;
        let outcome = match joined {
            Ok(o) => o,
            Err(e) => {
                error!(collection = %self.id, "writer commit task failed: {e}");
                self.writer = None;
                for (reply, permit) in slots {
                    let _ = reply.send(Err(WriteError::Internal("writer commit panicked")));
                    drop(permit);
                }
                return;
            }
        };

        self.writer = outcome.writer;
        if let Some(s) = outcome.new_snapshot {
            self.published.store(s);
        }
        let mut replies: Vec<Option<Reply>> = (0..n).map(|_| None).collect();
        let mut commits = 0u64;
        let mut acked = 0u64;
        for done in outcome.done {
            self.on_committed(done.generation, done.cdx_len, &done.entries);
            commits += 1;
            let mut it = done.entries.into_iter();
            for i in done.requests {
                let want = lens.get(i).copied().unwrap_or(0);
                let entries: Vec<Entry<'static>> = it.by_ref().take(want).collect();
                let reply = if entries.len() == want {
                    acked = acked.saturating_add(u64::try_from(want).unwrap_or(u64::MAX));
                    Ok(Committed {
                        generation: done.generation,
                        entries,
                    })
                } else {
                    Err(WriteError::Internal("commit returned fewer entries"))
                };
                if let Some(slot) = replies.get_mut(i) {
                    *slot = Some(reply);
                }
            }
        }
        for (range, err) in outcome.failed {
            for i in range {
                if let Some(slot) = replies.get_mut(i) {
                    *slot = Some(Err(err.clone()));
                }
            }
        }
        self.published.commits.fetch_add(commits, Ordering::Release);
        self.published.records.fetch_add(acked, Ordering::Release);
        if let Some(w) = &self.writer {
            let len = w.cdx_len();
            let before = self.published.cdx_len.swap(len, Ordering::Release);
            if before != len {
                // The CDX is fsynced up to `len`: the indexer tails it. This also covers a reopen
                // that kept lines of a failed attempt.
                self.link.wake(&self.feed);
            }
        }
        for ((reply, permit), r) in slots.into_iter().zip(replies) {
            let r = r.unwrap_or(Err(WriteError::Internal("no result for request")));
            // A dropped receiver means the caller cancelled; the write stands.
            let _ = reply.send(r);
            drop(permit);
        }
        self.maybe_checkpoint().await;
    }

    /// Called once per successful commit, before the replies are sent. The secondary index does
    /// not use it: it tails the CDX (see `indexer.rs`).
    fn on_committed(&self, generation: u32, cdx_len: u64, entries: &[Entry<'static>]) {
        debug!(
            collection = %self.id,
            generation,
            cdx_len,
            records = entries.len(),
            "committed"
        );
    }

    async fn maybe_checkpoint(&mut self) {
        let due = self.writer.as_ref().is_some_and(|w| {
            !w.is_poisoned()
                && w.cdx_len().saturating_sub(self.last_checkpoint_len) >= self.checkpoint_every
        });
        if !due {
            return;
        }
        let Some(mut w) = self.writer.take() else {
            return;
        };
        match tokio::task::spawn_blocking(move || {
            let r = w.checkpoint();
            let len = w.cdx_len();
            (w, len, r)
        })
        .await
        {
            Ok((w, len, r)) => {
                self.writer = Some(w);
                match r {
                    Ok(_) => self.last_checkpoint_len = len,
                    Err(e) => warn!(collection = %self.id, "checkpoint failed: {e}"),
                }
            }
            Err(e) => error!(collection = %self.id, "checkpoint task failed: {e}"),
        }
    }

    async fn final_checkpoint(&mut self) {
        let Some(mut w) = self.writer.take() else {
            return;
        };
        if w.is_poisoned() {
            return;
        }
        let id = self.id;
        match tokio::task::spawn_blocking(move || {
            let r = w.checkpoint();
            drop(w);
            r
        })
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!(collection = %id, "final checkpoint failed: {e}"),
            Err(e) => error!(collection = %id, "final checkpoint task failed: {e}"),
        }
    }
}

/// Opens the collection again, from disk.
///
/// Exclusivity (precondition of `Collection::open`): the writer task is the only owner of the
/// collection's `Writer` and the only code path that mutates the collection directory in M17, and
/// the old writer was dropped before this call. The admin mutex is deliberately not taken: M20's
/// rebuild holds it while waiting for the writer task, so the task waiting for it would deadlock.
/// M20 must abort or drop an in-progress `Rebuilder` before the writer task reopens.
fn reopen(
    dir: &Path,
    opts: &Options,
    resolver: &StoreResolver,
) -> Result<(Writer, Arc<Snapshot>), masstuffy_collection::Error> {
    let (coll, report) = Collection::open(dir, opts, &RecoveryOptions::default(), resolver)?;
    if !report.is_clean() {
        warn!(dir = %dir.display(), ?report, "collection reopened with repairs");
    }
    Ok(coll.into_parts())
}

/// The item range of requests `r`, from the prefix sums `offsets`.
fn item_range(offsets: &[usize], r: &Range<usize>) -> Option<Range<usize>> {
    Some(*offsets.get(r.start)?..*offsets.get(r.end)?)
}

fn commit_range(
    w: &mut Writer,
    items: &[Prepared],
    range: Option<Range<usize>>,
    faults: &Faults,
) -> Commit {
    if let Some(e) = injected(faults) {
        return Err(Failure::Commit(e));
    }
    match range.and_then(|r| items.get(r)) {
        Some(slice) => w.commit(slice).map_err(Failure::Commit),
        None => Err(Failure::Internal),
    }
}

type Commit = Result<Vec<Entry<'static>>, Failure>;

enum Failure {
    Commit(CommitError),
    Internal,
}

impl Failure {
    /// True if the writer is poisoned after this failure. (`CommitError::Poisoned` is only
    /// returned for an already poisoned writer, except for an injected one in tests.)
    fn poisoned(&self, w: &Writer) -> bool {
        w.is_poisoned() || matches!(self, Failure::Commit(CommitError::Poisoned))
    }

    fn into_write_error(self) -> WriteError {
        match self {
            Failure::Commit(e) => WriteError::Commit(Arc::new(e)),
            Failure::Internal => WriteError::Internal("bad request ranges"),
        }
    }
}

/// Runs in the blocking pool. See the plan: reopen a lost or poisoned writer, commit the group;
/// on a poisoning error reopen and retry once; on another error commit each request alone.
fn commit_blocking(
    writer: Option<Writer>,
    items: Vec<Prepared>,
    lens: &[usize],
    target: &Target,
    drop_first: bool,
    faults: &Faults,
) -> BlockingOutcome {
    let dir = target.dir.as_path();
    let n = lens.len();
    let mut offsets = Vec::with_capacity(n + 1);
    offsets.push(0usize);
    let mut acc = 0usize;
    for &l in lens {
        acc = acc.saturating_add(l);
        offsets.push(acc);
    }
    let all = 0..n;
    let mut out = BlockingOutcome {
        writer: None,
        new_snapshot: None,
        done: Vec::new(),
        failed: Vec::new(),
    };

    let mut w = match writer {
        Some(w) if !drop_first && !w.is_poisoned() => w,
        other => {
            drop(other);
            match reopen(&target.dir, &target.opts, &target.resolver) {
                Ok((w, s)) => {
                    out.new_snapshot = Some(s);
                    w
                }
                Err(e) => {
                    out.failed.push((all, WriteError::Reopen(Arc::new(e))));
                    return out;
                }
            }
        }
    };

    match commit_range(&mut w, &items, item_range(&offsets, &all), faults) {
        Ok(entries) => {
            out.done.push(CommitDone {
                generation: w.generation(),
                cdx_len: w.cdx_len(),
                entries,
                requests: all,
            });
            out.writer = Some(w);
        }
        Err(f) if f.poisoned(&w) => {
            warn!(dir = %dir.display(), "commit failed, reopening the collection");
            drop(f);
            drop(w);
            match reopen(&target.dir, &target.opts, &target.resolver) {
                Ok((mut w2, s)) => {
                    out.new_snapshot = Some(s);
                    // Retry the unacknowledged batch once (at-least-once: a prefix may be
                    // duplicated if the failed attempt left it durable).
                    match commit_range(&mut w2, &items, item_range(&offsets, &all), faults) {
                        Ok(entries) => out.done.push(CommitDone {
                            generation: w2.generation(),
                            cdx_len: w2.cdx_len(),
                            entries,
                            requests: all,
                        }),
                        Err(f) => out.failed.push((all, f.into_write_error())),
                    }
                    out.writer = Some(w2);
                }
                Err(re) => out.failed.push((all, WriteError::Reopen(Arc::new(re)))),
            }
        }
        Err(f) if n > 1 => {
            // Non-poisoning error: nothing was written. Isolate the culprit by committing each
            // request alone, in order.
            debug!(dir = %dir.display(), "group commit rejected; committing requests alone");
            drop(f);
            for i in 0..n {
                let one = i..i + 1;
                match commit_range(&mut w, &items, item_range(&offsets, &one), faults) {
                    Ok(entries) => out.done.push(CommitDone {
                        generation: w.generation(),
                        cdx_len: w.cdx_len(),
                        entries,
                        requests: one,
                    }),
                    Err(f) => {
                        let poisoned = f.poisoned(&w);
                        out.failed.push((one, f.into_write_error()));
                        if poisoned {
                            if i + 1 < n {
                                out.failed.push((
                                    i + 1..n,
                                    WriteError::Commit(Arc::new(CommitError::Poisoned)),
                                ));
                            }
                            break;
                        }
                    }
                }
            }
            out.writer = Some(w);
        }
        Err(f) => {
            out.failed.push((all, f.into_write_error()));
            out.writer = Some(w);
        }
    }
    out
}
