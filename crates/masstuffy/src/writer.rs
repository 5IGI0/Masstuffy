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
    Collection, CollectionId, CommitError, Manifest, OldGeneration, Options, Prepared,
    PreparedTombstone, RebuildReport, Rebuilt, RecoveryOptions, Snapshot, Writer, set_slug,
};
use masstuffy_durable::DirLock;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};
use tracing::{debug, error, warn};

use crate::indexer::{IndexFeed, IndexLink};
use crate::rebuild::RebuildSlot;
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

/// Why a request got no `Committed`.
pub(crate) enum Rejected {
    /// The commit failed.
    Error(WriteError),
    /// A request prepared for another generation than the writer's (a rebuild switched while it
    /// was queued): nothing was written; the items are handed back to be prepared again.
    Stale(Vec<Prepared>),
}

pub(crate) type Reply = Result<Committed, Rejected>;

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
    /// Rebuild: after the requests queued before this message, take the `Rebuilder` of the slot
    /// and run its `finish` (final delta, manifest swap), then publish the new generation.
    Finish(oneshot::Sender<FinishReply>),
    /// Record deletion: append the tombstones (one CDX append + fsync), reply after the fsync.
    Tombstone(TombstoneRequest),
    /// Rename: rewrite the manifest slug and publish the new snapshot.
    SetSlug(SlugRequest),
}

/// Prepared tombstones of one `CollectionHandle::delete` call.
pub(crate) struct TombstoneRequest {
    pub(crate) items: Vec<PreparedTombstone>,
    pub(crate) reply: oneshot::Sender<Result<(), WriteError>>,
}

/// A rename.
pub(crate) struct SlugRequest {
    pub(crate) slug: Arc<str>,
    pub(crate) reply: oneshot::Sender<Result<(), SlugFail>>,
}

/// Whether the namespace holds the new slug after a failed rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlugApplied {
    Yes,
    No,
    Unknown,
}

/// A failed rename.
pub(crate) struct SlugFail {
    pub(crate) source: masstuffy_collection::Error,
    pub(crate) applied: SlugApplied,
}

/// What the run loop does between groups.
enum Control {
    Finish(oneshot::Sender<FinishReply>),
    Tombstone(TombstoneRequest),
    SetSlug(SlugRequest),
}

/// Points of `delete_collection` where tests can "crash" it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeletePoint {
    /// The writer task is stopped, nothing was renamed.
    Stopped,
    /// The directory was moved to the trash (and the parent fsynced), the registry is untouched.
    Trashed,
}

/// A committed rebuild: what the rebuild unit needs to finish up.
pub(crate) struct FinishDone {
    pub(crate) report: RebuildReport,
    pub(crate) old: OldGeneration,
}

/// Why a `Msg::Finish` did not commit a rebuild.
pub(crate) enum FinishFail {
    /// No `Rebuilder` in the slot (a reopen took it), or the writer was unavailable (lost or
    /// poisoned: the `Rebuilder` was aborted by the writer task).
    Aborted(&'static str),
    /// `finish` failed. `committed` as `Error::Rebuild::committed`.
    Failed {
        source: masstuffy_collection::Error,
        committed: bool,
    },
}

pub(crate) type FinishReply = Result<FinishDone, FinishFail>;

/// The snapshots readers can use. Invariant: `retired`, when `Some`, is the snapshot of the
/// generation that the last committed rebuild replaced, its files are still on disk, and
/// `retired.generation() < current.generation()`.
struct Snaps {
    current: Arc<Snapshot>,
    retired: Option<Arc<Snapshot>>,
}

/// What readers and `stats()` see of a collection. It holds no `Sender`, so the writer task
/// holding it does not keep its own channel open.
pub(crate) struct Published {
    snapshot: RwLock<Snaps>,
    pub(crate) commits: AtomicU64,
    pub(crate) records: AtomicU64,
    pub(crate) cdx_len: AtomicU64,
}

impl Published {
    pub(crate) fn new(snapshot: Arc<Snapshot>, cdx_len: u64) -> Published {
        Published {
            snapshot: RwLock::new(Snaps {
                current: snapshot,
                retired: None,
            }),
            commits: AtomicU64::new(0),
            records: AtomicU64::new(0),
            cdx_len: AtomicU64::new(cdx_len),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Snaps> {
        // A poisoned lock is recovered: the guarded value is two `Arc`s, always consistent.
        self.snapshot.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Snaps> {
        self.snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The current snapshot: one read lock and an `Arc` clone.
    pub(crate) fn load(&self) -> Arc<Snapshot> {
        Arc::clone(&self.read().current)
    }

    /// The current snapshot and the fsynced CDX length, read together: the length is loaded while
    /// the snapshot read lock is held, and `switch` stores a new-generation snapshot and its length
    /// under the write lock.
    pub(crate) fn tail(&self) -> (Arc<Snapshot>, u64) {
        let g = self.read();
        let len = self.cdx_len.load(Ordering::Acquire);
        (Arc::clone(&g.current), len)
    }

    /// Replaces the current snapshot after a reopen. A different generation means the reopen
    /// crossed a switch (`Collection::open` deleted the other generation's files): the retired
    /// snapshot is dropped.
    pub(crate) fn store(&self, s: Arc<Snapshot>) {
        let mut g = self.write();
        if s.generation() != g.current.generation() {
            g.retired = None;
        }
        g.current = s;
    }

    /// A committed rebuild: `s` becomes current with its CDX length (both under one write guard),
    /// the previous snapshot becomes the retired one.
    pub(crate) fn switch(&self, s: Arc<Snapshot>, cdx_len: u64) {
        let mut g = self.write();
        self.cdx_len.store(cdx_len, Ordering::Release);
        let old = std::mem::replace(&mut g.current, s);
        g.retired = Some(old);
    }

    /// Forgets the retired snapshot (the index has switched; its files are about to be removed).
    pub(crate) fn clear_retired(&self) {
        self.write().retired = None;
    }

    /// The snapshot of `generation`: the current one, or the retired one. `Err` holds the current
    /// generation.
    pub(crate) fn load_for(&self, generation: u32) -> Result<Arc<Snapshot>, u32> {
        let g = self.read();
        if g.current.generation() == generation {
            return Ok(Arc::clone(&g.current));
        }
        match &g.retired {
            Some(r) if r.generation() == generation => Ok(Arc::clone(r)),
            _ => Err(g.current.generation()),
        }
    }
}

/// Hooks that let tests hold the writer task back or make it lose its writer.
#[cfg(test)]
pub(crate) struct TestHooks {
    /// When `Some`, the task acquires and forgets one permit before draining each group.
    pub(crate) hold: std::sync::Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Consumed by the next commit: the writer is dropped first so the reopen path runs.
    pub(crate) drop_writer_next: std::sync::atomic::AtomicBool,
    /// One entry is consumed by each `Writer::commit` attempt (before the real commit, which
    /// is then skipped): the injected failure.
    pub(crate) panic_on_exit: std::sync::atomic::AtomicBool,
    pub(crate) script: std::sync::Mutex<std::collections::VecDeque<Fault>>,
    /// Last rebuild point reached by a rebuild unit.
    pub(crate) rebuild_reached: tokio::sync::watch::Sender<Option<RebuildPoint>>,
    /// The unit waits for one permit when it reaches that point.
    pub(crate) rebuild_pause: std::sync::Mutex<Option<(RebuildPoint, Arc<tokio::sync::Semaphore>)>>,
    /// The unit returns `Internal("injected rebuild crash")` at that point, with no cleanup.
    pub(crate) rebuild_crash: std::sync::Mutex<Option<RebuildPoint>>,
    /// `put_stage` waits for one permit after prepare, before submit.
    pub(crate) after_prepare: std::sync::Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Number of `put_stage` calls that reached the `after_prepare` point.
    pub(crate) after_prepare_reached: tokio::sync::watch::Sender<u64>,
    /// The next copy round pauses (slot mutex held) until released.
    pub(crate) copy_gate: std::sync::Mutex<Option<Arc<CopyGate>>>,
    /// `delete_collection` returns `Internal("injected delete crash")` at that point, with no
    /// cleanup.
    pub(crate) delete_crash: std::sync::Mutex<Option<DeletePoint>>,
    /// Last delete point reached.
    pub(crate) delete_reached: tokio::sync::watch::Sender<Option<DeletePoint>>,
}

/// `(entered, released)` of a paused copy round.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct CopyGate {
    pub(crate) state: std::sync::Mutex<(bool, bool)>,
    pub(crate) cv: std::sync::Condvar,
}

#[cfg(test)]
impl CopyGate {
    pub(crate) fn wait_entered(&self) {
        let mut g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        while !g.0 {
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub(crate) fn release(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
        self.cv.notify_all();
    }
}

/// Points of a rebuild unit where tests can pause it or make it "crash".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RebuildPoint {
    Started,
    Copied,
    Finished,
    Switched,
}

#[cfg(test)]
impl Default for TestHooks {
    fn default() -> Self {
        TestHooks {
            hold: std::sync::Mutex::new(None),
            drop_writer_next: std::sync::atomic::AtomicBool::new(false),
            panic_on_exit: std::sync::atomic::AtomicBool::new(false),
            script: std::sync::Mutex::new(std::collections::VecDeque::new()),
            rebuild_reached: tokio::sync::watch::channel(None).0,
            rebuild_pause: std::sync::Mutex::new(None),
            rebuild_crash: std::sync::Mutex::new(None),
            after_prepare: std::sync::Mutex::new(None),
            after_prepare_reached: tokio::sync::watch::channel(0).0,
            copy_gate: std::sync::Mutex::new(None),
            delete_crash: std::sync::Mutex::new(None),
            delete_reached: tokio::sync::watch::channel(None).0,
        }
    }
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
    /// Blocking: called by a copy round with the slot mutex held; pauses once if a gate is set.
    pub(crate) fn pause_in_copy(&self) {
        let gate = self
            .copy_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(g) = gate {
            let mut st = g.state.lock().unwrap_or_else(PoisonError::into_inner);
            st.0 = true;
            g.cv.notify_all();
            while !st.1 {
                st = g.cv.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        }
    }

    pub(crate) async fn wait_after_prepare(&self) {
        self.after_prepare_reached.send_modify(|n| *n += 1);
        let sem = self
            .after_prepare
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(s) = sem
            && let Ok(p) = s.acquire().await
        {
            p.forget();
        }
    }

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
    pub(crate) rebuild: Arc<RebuildSlot>,
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
    /// The rebuild state shared with the rebuild unit.
    rebuild: Arc<RebuildSlot>,
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
    slot: Arc<RebuildSlot>,
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
            rebuild: p.rebuild,
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
                    Some(Msg::Finish(r)) => {
                        self.run_control(Control::Finish(r)).await;
                        continue;
                    }
                    Some(Msg::Tombstone(r)) => {
                        self.run_control(Control::Tombstone(r)).await;
                        continue;
                    }
                    Some(Msg::SetSlug(r)) => {
                        self.run_control(Control::SetSlug(r)).await;
                        continue;
                    }
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
            // A control message read while draining runs right after this group (`pending` and
            // `control` are never both set: the loop stops at the first of them).
            let mut control: Option<Control> = None;
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
                    Ok(Msg::Finish(r)) => {
                        control = Some(Control::Finish(r));
                        break;
                    }
                    Ok(Msg::Tombstone(r)) => {
                        control = Some(Control::Tombstone(r));
                        break;
                    }
                    Ok(Msg::SetSlug(r)) => {
                        control = Some(Control::SetSlug(r));
                        break;
                    }
                    Ok(Msg::Shutdown(ack)) => stop = Some(Some(ack)),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => stop = Some(None),
                }
            }
            self.commit_group(group).await;
            if let Some(c) = control.take() {
                self.run_control(c).await;
            }
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

    async fn run_control(&mut self, c: Control) {
        match c {
            Control::Finish(r) => self.run_finish(r).await,
            Control::Tombstone(r) => self.run_tombstone(r).await,
            Control::SetSlug(r) => self.run_set_slug(r).await,
        }
    }

    fn target(&self) -> Target {
        Target {
            dir: self.dir.clone(),
            opts: self.opts,
            resolver: Arc::clone(&self.resolver),
            slot: Arc::clone(&self.rebuild),
        }
    }

    /// Appends the tombstones (one CDX append and fsync) in the blocking pool, publishes the new
    /// CDX length and replies after the fsync.
    async fn run_tombstone(&mut self, req: TombstoneRequest) {
        let TombstoneRequest { items, reply } = req;
        #[cfg(test)]
        let drop_first = self
            .hooks
            .drop_writer_next
            .swap(false, std::sync::atomic::Ordering::AcqRel);
        #[cfg(not(test))]
        let drop_first = false;
        let writer = self.writer.take();
        let target = self.target();
        let joined = tokio::task::spawn_blocking(move || {
            tombstone_blocking(writer, &items, &target, drop_first)
        })
        .await;
        let out = match joined {
            Ok(o) => o,
            Err(e) => {
                error!(collection = %self.id, "tombstone task failed: {e}");
                self.writer = None;
                let _ = reply.send(Err(WriteError::Internal("tombstone task panicked")));
                return;
            }
        };
        self.writer = out.writer;
        if let Some(s) = out.new_snapshot {
            self.published.store(s);
            // A reopened writer starts a fresh checkpoint interval, as in `commit_group`.
            if let Some(w) = &self.writer {
                self.last_checkpoint_len = w.cdx_len();
            }
        }
        if let Some(w) = &self.writer {
            let len = w.cdx_len();
            if self.published.cdx_len.swap(len, Ordering::Release) != len {
                self.link.wake(&self.feed);
            }
        }
        // A dropped receiver means the caller cancelled; the write stands.
        let _ = reply.send(out.result);
        self.maybe_checkpoint().await;
    }

    /// Rewrites the manifest slug in the blocking pool and publishes the new snapshot (same
    /// generation, so the retired snapshot is kept). The writer task is the only place that
    /// reopens a collection, and `Collection::open` sweeps `.tmp-*` in the directory: it must
    /// never run concurrently with the `atomic_write` of the manifest.
    async fn run_set_slug(&mut self, req: SlugRequest) {
        let SlugRequest { slug, reply } = req;
        let writer = self.writer.take();
        let target = self.target();
        let current = self.published.load();
        let lock = Arc::clone(&self._lock);
        let joined = tokio::task::spawn_blocking(move || {
            let _lock = lock;
            slug_blocking(writer, &current, &slug, &target)
        })
        .await;
        let out = match joined {
            Ok(o) => o,
            Err(e) => {
                error!(collection = %self.id, "rename task failed: {e}");
                self.writer = None;
                let _ = reply.send(Err(SlugFail {
                    source: masstuffy_collection::Error::Internal("rename task panicked"),
                    applied: SlugApplied::Unknown,
                }));
                return;
            }
        };
        self.writer = out.writer;
        if let Some(s) = out.publish {
            self.published.store(s);
        }
        if out.reopened
            && let Some(w) = &self.writer
        {
            let len = w.cdx_len();
            self.last_checkpoint_len = len;
            if self.published.cdx_len.swap(len, Ordering::Release) != len {
                self.link.wake(&self.feed);
            }
        }
        let _ = reply.send(out.result);
    }

    /// Answers `Stale` to every request prepared for another generation than the writer's, and
    /// returns the others. Without a writer (lost) nothing is filtered.
    fn reject_stale(&self, group: Vec<Request>) -> Vec<Request> {
        let Some(w) = &self.writer else {
            return group;
        };
        let generation = w.generation();
        let id = self.id;
        let mut keep = Vec::with_capacity(group.len());
        for r in group {
            let stale = r.items.iter().any(|p| {
                let c = p.context();
                c.generation() != generation || c.collection() != id
            });
            if stale {
                let Request {
                    items,
                    reply,
                    permit,
                    ..
                } = r;
                let _ = reply.send(Err(Rejected::Stale(items)));
                drop(permit);
            } else {
                keep.push(r);
            }
        }
        keep
    }

    async fn commit_group(&mut self, group: Vec<Request>) {
        let group = self.reject_stale(group);
        if group.is_empty() {
            return;
        }
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
        let target = self.target();
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
                    let _ = reply.send(Err(Rejected::Error(WriteError::Internal(
                        "writer commit panicked",
                    ))));
                    drop(permit);
                }
                return;
            }
        };

        self.writer = outcome.writer;
        if let Some(s) = outcome.new_snapshot {
            self.published.store(s);
        }
        let mut replies: Vec<Option<Result<Committed, WriteError>>> =
            (0..n).map(|_| None).collect();
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
            let r = r
                .unwrap_or(Err(WriteError::Internal("no result for request")))
                .map_err(Rejected::Error);
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

    /// Runs the final step of a rebuild in the blocking pool, then publishes the result. The
    /// blocking job owns the writer meanwhile (`self.writer` is `None`, no request is drained).
    async fn run_finish(&mut self, reply: oneshot::Sender<FinishReply>) {
        let writer = self.writer.take();
        let target = self.target();
        let lock = Arc::clone(&self._lock);
        let joined = tokio::task::spawn_blocking(move || {
            let _lock = lock;
            finish_blocking(writer, &target)
        })
        .await;
        let out = match joined {
            Ok(o) => o,
            Err(e) => {
                error!(collection = %self.id, "rebuild finish task failed: {e}");
                self.writer = None;
                let _ = reply.send(Err(FinishFail::Aborted("finish task panicked")));
                return;
            }
        };
        self.writer = out.writer;
        match out.publish {
            Some(Publish::Switch(snapshot)) => {
                if let Some(w) = &self.writer {
                    let len = w.cdx_len();
                    self.published.switch(snapshot, len);
                    self.last_checkpoint_len = len;
                    // The indexer resolves the switch (resume at the staged checkpoint).
                    self.link.wake(&self.feed);
                }
            }
            Some(Publish::Reopened(snapshot)) => {
                self.published.store(snapshot);
                if let Some(w) = &self.writer {
                    let len = w.cdx_len();
                    self.last_checkpoint_len = len;
                    if self.published.cdx_len.swap(len, Ordering::Release) != len {
                        self.link.wake(&self.feed);
                    }
                }
            }
            None => {}
        }
        // A dropped receiver can only mean the unit task died: the old files are then removed by
        // the next open.
        let _ = reply.send(out.result);
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
/// collection's `Writer`, the old writer was dropped before this call, and the slot mutex is
/// held for the whole open, so no `Rebuilder::start` or `copy_until` runs meanwhile. A running
/// `Rebuilder` is cancelled (its copy stops at the next CDX line), then dropped here: its files
/// are deleted by the open below. The admin mutex is deliberately not taken: a rebuild holds it
/// while waiting for this task, so the task waiting for it would deadlock.
fn reopen(
    dir: &Path,
    opts: &Options,
    resolver: &StoreResolver,
    slot: &RebuildSlot,
) -> Result<(Writer, Arc<Snapshot>), masstuffy_collection::Error> {
    slot.cancel.store(true, Ordering::SeqCst);
    let mut g = slot.rb.lock().unwrap_or_else(PoisonError::into_inner);
    drop(g.take());
    let r = Collection::open(dir, opts, &RecoveryOptions::default(), resolver);
    drop(g);
    let (coll, report) = r?;
    if !report.is_clean() {
        warn!(dir = %dir.display(), ?report, "collection reopened with repairs");
    }
    Ok(coll.into_parts())
}

/// What the writer task publishes after a finish job.
enum Publish {
    /// The rebuild committed: the new generation's snapshot.
    Switch(Arc<Snapshot>),
    /// The writer was reopened from disk.
    Reopened(Arc<Snapshot>),
}

struct FinishOutcome {
    writer: Option<Writer>,
    publish: Option<Publish>,
    result: FinishReply,
}

/// Runs in the blocking pool: takes the `Rebuilder` from the slot and runs its `finish`.
fn finish_blocking(writer: Option<Writer>, target: &Target) -> FinishOutcome {
    let aborted = |writer, why| FinishOutcome {
        writer,
        publish: None,
        result: Err(FinishFail::Aborted(why)),
    };
    let rb = target
        .slot
        .rb
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let Some(rb) = rb else {
        return aborted(writer, "rebuild was cancelled");
    };
    let mut w = match writer {
        Some(w) if !w.is_poisoned() => w,
        other => {
            if let Err(e) = rb.abort() {
                warn!(dir = %target.dir.display(), "cannot remove the files of an aborted rebuild: {e}");
            }
            return aborted(other, "writer unavailable");
        }
    };
    let new_gen = rb.report().generation;
    match rb.finish(&mut w) {
        Ok(Rebuilt {
            writer: nw,
            snapshot,
            report,
            old,
            ..
        }) => {
            drop(w); // retired
            FinishOutcome {
                writer: Some(nw),
                publish: Some(Publish::Switch(snapshot)),
                result: Ok(FinishDone { report, old }),
            }
        }
        Err(source) if !w.is_poisoned() => {
            // Nothing committed: the new generation's files are leftovers (only disk space).
            if let Err(e) = w.remove_stale_generations() {
                warn!(dir = %target.dir.display(), "cannot remove the files of a failed rebuild: {e}");
            }
            FinishOutcome {
                writer: Some(w),
                publish: None,
                result: Err(FinishFail::Failed {
                    source,
                    committed: false,
                }),
            }
        }
        Err(source) => {
            // The manifest swap may or may not have happened: only a reopen can tell.
            drop(w);
            match reopen(&target.dir, &target.opts, &target.resolver, &target.slot) {
                Ok((w2, s)) => {
                    let committed = s.generation() == new_gen;
                    FinishOutcome {
                        writer: Some(w2),
                        publish: Some(Publish::Reopened(s)),
                        result: Err(FinishFail::Failed { source, committed }),
                    }
                }
                Err(re) => {
                    error!(dir = %target.dir.display(), "reopen after a failed rebuild failed: {re}");
                    FinishOutcome {
                        writer: None,
                        publish: None,
                        result: Err(FinishFail::Failed {
                            source,
                            committed: false,
                        }),
                    }
                }
            }
        }
    }
}

struct TombOutcome {
    writer: Option<Writer>,
    new_snapshot: Option<Arc<Snapshot>>,
    result: Result<(), WriteError>,
}

/// Runs in the blocking pool: reopens a lost, poisoned or test-dropped writer, then appends the
/// tombstones. After a poisoning error the collection is reopened and the append retried once (a
/// duplicated `T` line is harmless); any other error keeps the writer.
fn tombstone_blocking(
    writer: Option<Writer>,
    items: &[PreparedTombstone],
    target: &Target,
    drop_first: bool,
) -> TombOutcome {
    let mut out = TombOutcome {
        writer: None,
        new_snapshot: None,
        result: Ok(()),
    };
    let mut w = match writer {
        Some(w) if !drop_first && !w.is_poisoned() => w,
        other => {
            drop(other);
            match reopen(&target.dir, &target.opts, &target.resolver, &target.slot) {
                Ok((w, s)) => {
                    out.new_snapshot = Some(s);
                    w
                }
                Err(e) => {
                    out.result = Err(WriteError::Reopen(Arc::new(e)));
                    return out;
                }
            }
        }
    };
    match w.tombstone(items) {
        Ok(_) => out.writer = Some(w),
        Err(e) if w.is_poisoned() || matches!(e, CommitError::Poisoned) => {
            warn!(dir = %target.dir.display(), "tombstone failed ({e}), reopening the collection");
            drop(w);
            match reopen(&target.dir, &target.opts, &target.resolver, &target.slot) {
                Ok((mut w2, s)) => {
                    out.new_snapshot = Some(s);
                    if let Err(e) = w2.tombstone(items) {
                        out.result = Err(WriteError::Commit(Arc::new(e)));
                    }
                    out.writer = Some(w2);
                }
                Err(re) => out.result = Err(WriteError::Reopen(Arc::new(re))),
            }
        }
        Err(e) => {
            out.result = Err(WriteError::Commit(Arc::new(e)));
            out.writer = Some(w);
        }
    }
    out
}

struct SlugOutcome {
    writer: Option<Writer>,
    publish: Option<Arc<Snapshot>>,
    /// The writer was reopened from disk.
    reopened: bool,
    result: Result<(), SlugFail>,
}

/// Runs in the blocking pool: `set_slug`; on failure the writer is dropped and the collection is
/// reopened, which tells whether the namespace holds the new slug.
fn slug_blocking(
    writer: Option<Writer>,
    current: &Snapshot,
    slug: &str,
    target: &Target,
) -> SlugOutcome {
    match set_slug(current, slug) {
        Ok(s) => SlugOutcome {
            writer,
            publish: Some(Arc::new(s)),
            reopened: false,
            result: Ok(()),
        },
        Err(source) => {
            drop(writer);
            match reopen(&target.dir, &target.opts, &target.resolver, &target.slot) {
                Ok((w, s)) => {
                    let applied = if s.manifest().slug() == slug {
                        SlugApplied::Yes
                    } else {
                        SlugApplied::No
                    };
                    SlugOutcome {
                        writer: Some(w),
                        publish: Some(s),
                        reopened: true,
                        result: Err(SlugFail { source, applied }),
                    }
                }
                Err(re) => {
                    error!(dir = %target.dir.display(), "reopen after a failed rename failed: {re}");
                    let applied = match Manifest::load(&target.dir) {
                        Ok(m) if m.slug() == slug => SlugApplied::Yes,
                        Ok(_) => SlugApplied::No,
                        Err(_) => SlugApplied::Unknown,
                    };
                    SlugOutcome {
                        writer: None,
                        publish: None,
                        reopened: false,
                        result: Err(SlugFail { source, applied }),
                    }
                }
            }
        }
    }
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
            match reopen(&target.dir, &target.opts, &target.resolver, &target.slot) {
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
            match reopen(&target.dir, &target.opts, &target.resolver, &target.slot) {
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use masstuffy_collection::{
        Codec, CollectionId, CreateSpec, DEFAULT_SPLIT_THRESHOLD, DictResolver, RebuildOptions,
        ResolveError,
    };
    use masstuffy_zframe::{DecoderDict, EncoderDict};

    use super::*;
    use crate::testutil::TestDir;

    /// No dictionaries: enough for Codec::None collections.
    struct NoDicts;

    impl DictResolver for NoDicts {
        fn decoder(&self, id: NonZeroU32) -> Result<Arc<DecoderDict>, ResolveError> {
            Err(ResolveError::NotFound(id.get()))
        }

        fn encoder(&self, id: NonZeroU32, _level: i32) -> Result<Arc<EncoderDict>, ResolveError> {
            Err(ResolveError::NotFound(id.get()))
        }
    }

    #[test]
    fn published_switch_and_retired() {
        let td = TestDir::new();
        let opts = Options::default();
        let (mut c, _) = Collection::create(
            &td.path().join("c"),
            CreateSpec {
                uuid: CollectionId::from_u128(7),
                slug: "p",
                codec: Codec::None,
                split_threshold: DEFAULT_SPLIT_THRESHOLD,
            },
            &opts,
            &NoDicts,
        )
        .unwrap();
        let old = Arc::clone(c.snapshot());
        let p = Published::new(Arc::clone(&old), c.writer().cdx_len());
        let (_, gone) = c
            .rebuild(Codec::None, &opts, RebuildOptions::default(), &NoDicts)
            .unwrap();
        let new = Arc::clone(c.snapshot());
        let new_len = c.writer().cdx_len();
        assert_eq!((old.generation(), new.generation()), (1, 2));
        assert_ne!(p.tail().1, new_len);

        p.switch(Arc::clone(&new), new_len);
        let (cur, len) = p.tail();
        assert!(Arc::ptr_eq(&cur, &new));
        assert_eq!(len, new_len);
        assert!(Arc::ptr_eq(&p.load_for(1).unwrap(), &old));
        assert!(Arc::ptr_eq(&p.load_for(2).unwrap(), &new));
        assert_eq!(p.load_for(3).err(), Some(2));

        p.clear_retired();
        assert_eq!(p.load_for(1).err(), Some(2));
        assert!(Arc::ptr_eq(&p.load(), &new));

        // `store` of the same generation keeps the retired snapshot; another one drops it.
        let p1 = Published::new(Arc::clone(&old), 0);
        p1.switch(Arc::clone(&new), new_len);
        p1.store(Arc::clone(&new));
        assert!(p1.load_for(1).is_ok());
        let p2 = Published::new(Arc::clone(&old), 0);
        p2.switch(Arc::clone(&new), new_len);
        p2.store(Arc::clone(&old));
        assert_eq!(p2.load_for(2).err(), Some(1));
        drop(gone);
    }
}
