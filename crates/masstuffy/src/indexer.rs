//! The secondary indexer: one task for the whole repository that keeps the index equal to the
//! CDX of every loaded collection.
//!
//! The writer task of a collection never sends entries: after its CDX fsync it publishes the new
//! fsynced length (`Published::cdx_len`) and wakes the indexer. The indexer **tails the CDX** of
//! each woken collection from its cursor (the index checkpoint, kept in memory between passes)
//! up to the published length, turns the lines into one atomic batch with a `Checkpoint` op per
//! collection, and applies it. Normal operation, catch-up at open, retry after a failed apply and
//! re-indexing after a repair are the same mechanism (spec 04 4.2c step 5, 4.7; spec 05 I8, 5.3).
//!
//! # Wake invariant
//!
//! A collection appears at most once in the [`Hub`] (its key is in the hub's `queued` set, kept
//! under the same mutex as the queue, so test-and-set plus push is one critical section, and the
//! clear happens on pop under that mutex). `wake` pushes before it `try_send`s `IndexCmd::Wake`. Whenever the hub is non-empty, either an `IndexCmd` is
//! in the channel or the indexer is running and looks at the hub before its next blocking
//! `recv`: the loop in [`Indexer::run`] checks the hub after every command batch. A `Wake`
//! dropped because the channel is full therefore never loses work.
//!
//! Ordering rule: the writer stores `cdx_len` **before** it takes the hub mutex to test-and-set,
//! and the indexer reads `Published::tail` **after** popping under the mutex. So either the
//! indexer's read sees the new length, or the writer's test-and-set happens after the pop and
//! queues the feed again. The mutex provides the ordering; no fences are needed.
//!
//! # Rebuild and delete commands
//!
//! A rebuild (`rebuild.rs`) talks to the indexer with three commands. [`IndexCmd::Stage`] runs at
//! rebuild start: it clears any staged rows of the new generation and puts
//! `Checkpoint { new generation, start of the new CDX }` in the staged slot, so that after the
//! manifest switch resolution rule 4 (`active < target` with a staged checkpoint) resumes there,
//! indexes the new CDX as staged rows and activates the generation with `ReplaceGeneration` in the
//! batch of the last chunk: queries never see a half-staged generation. [`IndexCmd::Unstage`]
//! undoes it for an abandoned rebuild. [`IndexCmd::AwaitActive`] replies once the collection is
//! indexed with the new generation active. Both `Stage` and `Unstage` run between passes
//! (`admin_ops`), set `purge_pending`, and never touch the in-memory `Track` (its cursor stays on
//! the old generation until the snapshot switches). The writer task stores a new-generation
//! snapshot and its `cdx_len` together (`Published::switch`), and a rebuild never runs while
//! `rebuild_index` holds the admin mutexes.
//!
//! [`IndexCmd::Forget`] (`delete_collection`) runs between passes too. It removes the collection
//! from the hub and from the tracks **before** its blocking job (so no later pass can recreate
//! the track), then applies `DropCollection` (durable, structural). If that apply fails, the
//! orphan GC of the next pass drops the rows: the track is gone and the key is not in `keep`. The
//! caller guarantees that the collection's writer task has exited, so nothing can wake the feed
//! again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use masstuffy_cdx::Kind;
use masstuffy_collection::{CollectionId, Snapshot};
use masstuffy_durable::DirLock;
use masstuffy_index::{Checkpoint, CollectionKey, CollectionState, Op, Position, SecondaryIndex};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::error::Error;
use crate::index::IndexBackend;
use crate::writer::Published;

/// Channel capacity of `IndexCmd` (wakes are coalesced by the hub, commands are rare).
pub(crate) const INDEX_QUEUE: usize = 64;
/// Most feeds handled by one pass (one `apply`).
const MAX_JOBS_PER_PASS: usize = 64;
/// Entries reclaimed per purge step.
const PURGE_STEP: usize = 4096;

/// What one collection publishes to the indexer. One per loaded collection.
pub(crate) struct IndexFeed {
    pub(crate) id: CollectionId,
    pub(crate) key: CollectionKey,
    /// Snapshot and fsynced CDX length: the source of truth for the tail.
    pub(crate) published: Arc<Published>,
    /// CDX length when the writer task was spawned (open or create). The first resolution never
    /// resumes from a checkpoint beyond it (repair/truncation guard, rule 6).
    pub(crate) initial_len: u64,
}

impl IndexFeed {
    pub(crate) fn new(id: CollectionId, published: Arc<Published>, initial_len: u64) -> IndexFeed {
        IndexFeed {
            id,
            key: CollectionKey::new(id.as_u128()),
            published,
            initial_len,
        }
    }
}

/// Queue and membership set, always changed together under the hub mutex.
#[derive(Default)]
struct HubState {
    queue: VecDeque<Arc<IndexFeed>>,
    queued: HashSet<CollectionKey>,
}

/// Collections with unindexed CDX. Each feed appears at most once (the `queued` set), so the queue
/// is bounded by the number of collections. The std mutex is a leaf lock: held only for a
/// push/pop, with no I/O and never across an `.await`.
#[derive(Default)]
pub(crate) struct Hub {
    dirty: Mutex<HubState>,
}

impl Hub {
    fn q(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.dirty.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues the feed unless it is already queued. Returns true if it was pushed.
    fn enqueue(&self, f: &Arc<IndexFeed>) -> bool {
        let mut g = self.q();
        if !g.queued.insert(f.key) {
            return false;
        }
        g.queue.push_back(Arc::clone(f));
        true
    }

    /// Pops a feed and clears its `queued` membership under the same lock: the caller reads the
    /// published length after this returns.
    fn pop_feed(&self) -> Option<Arc<IndexFeed>> {
        let mut g = self.q();
        let f = g.queue.pop_front()?;
        g.queued.remove(&f.key);
        Some(f)
    }

    fn is_empty(&self) -> bool {
        self.q().queue.is_empty()
    }

    /// Forgets `key`: removes it from the queue and the membership set.
    fn remove(&self, key: CollectionKey) {
        let mut g = self.q();
        g.queued.remove(&key);
        g.queue.retain(|f| f.key != key);
    }

    fn queued(&self) -> Vec<Arc<IndexFeed>> {
        self.q().queue.iter().cloned().collect()
    }
}

/// Held by every writer task: the hub and a clone of the indexer channel sender. The clone is also
/// the liveness token: the indexer's `recv` returns `None` only after `Core` and every writer
/// task are gone.
#[derive(Clone)]
pub(crate) struct IndexLink {
    hub: Arc<Hub>,
    pub(crate) tx: mpsc::Sender<IndexCmd>,
}

impl IndexLink {
    pub(crate) fn new(tx: mpsc::Sender<IndexCmd>) -> (IndexLink, Arc<Hub>) {
        let hub = Arc::new(Hub::default());
        (
            IndexLink {
                hub: Arc::clone(&hub),
                tx,
            },
            hub,
        )
    }

    /// Marks the feed dirty and wakes the indexer. Never blocks, never fails.
    pub(crate) fn wake(&self, feed: &Arc<IndexFeed>) {
        if self.hub.enqueue(feed) {
            // Push happened before this send. Full: a message is pending, so the indexer will
            // look at the hub. Closed: the indexer is gone, catch-up happens at the next open.
            let _ = self.tx.try_send(IndexCmd::Wake);
        }
    }
}

pub(crate) enum IndexCmd {
    /// The hub may be non-empty.
    Wake,
    /// Reply once every collection known now is indexed up to its current published CDX length
    /// and active.
    Barrier(oneshot::Sender<Result<(), Error>>),
    /// Force a re-index of every collection, drop orphans, then reply like `Barrier`.
    Reindex(oneshot::Sender<Result<(), Error>>),
    /// Rebuild start: clear any staged rows of `generation` and put `Checkpoint{generation, from}`
    /// in the staged slot, so that after the manifest switch the indexer resumes at `from`
    /// (resolution rule 4) instead of re-indexing. Applied only if the collection has an active
    /// generation `< generation` and no staged checkpoint of another generation is pending (an
    /// earlier rebuild whose index switch has not happened); replies `Ok(false)` without applying
    /// otherwise (unknown, re-index in progress, index ahead, or that pending staging).
    Stage {
        key: CollectionKey,
        generation: u32,
        from: u64,
        reply: oneshot::Sender<Result<bool, Error>>,
    },
    /// Abandoned rebuild: `DiscardStaged{generation}` if the active generation is `< generation`,
    /// else nothing.
    Unstage {
        key: CollectionKey,
        generation: u32,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Reply once the collection is indexed with `generation` active and up to the CDX length
    /// published when the command is handled (a barrier on one collection, same retry rule as
    /// `Barrier`).
    AwaitActive {
        key: CollectionKey,
        generation: u32,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// delete_collection: forget the collection (remove it from the hub and the tracks), then apply
    /// `DropCollection` (durable: structural). The caller guarantees that no writer task can wake
    /// the feed any more (its writer task has exited).
    Forget {
        key: CollectionKey,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Drain the hub completely, flush, ack, exit.
    Shutdown(oneshot::Sender<()>),
}

/// A `Stage`, `Unstage` or `Forget` waiting for its turn between passes.
enum AdminOp {
    Forget {
        key: CollectionKey,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    Stage {
        key: CollectionKey,
        generation: u32,
        from: u64,
        reply: oneshot::Sender<Result<bool, Error>>,
    },
    Unstage {
        key: CollectionKey,
        generation: u32,
        reply: oneshot::Sender<Result<(), Error>>,
    },
}

// ---- Resolution (where to resume a collection).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReindexReason {
    Unknown,
    Forced,
    IndexAhead,
    NoCheckpoint,
    BeyondCdx,
    Misaligned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Replay from `from`. `activate`: push `ReplaceGeneration` when caught up. `discard`: a stale
    /// staged generation to clear first.
    Resume {
        from: u64,
        activate: bool,
        discard: Option<u32>,
    },
    /// Re-index from the CDX header. `drop`: push `DropCollection` first.
    Reindex { drop: bool, reason: ReindexReason },
}

/// Decides how to bring the index of one collection up to date, from its index state, the
/// generation `target` of the snapshot, the CDX length `end`, the length `floor` the CDX had when
/// the writer task started (first resolution only) and whether a re-index is forced.
fn resolve(
    state: Option<CollectionState>,
    target: u32,
    end: u64,
    floor: Option<u64>,
    force: bool,
) -> Plan {
    let Some(st) = state else {
        return Plan::Reindex {
            drop: false,
            reason: ReindexReason::Unknown,
        };
    };
    let reindex = |reason| Plan::Reindex { drop: true, reason };
    if force {
        return reindex(ReindexReason::Forced);
    }
    if st.active_generation.is_some_and(|a| a > target) {
        return reindex(ReindexReason::IndexAhead);
    }
    let of_target = |c: Option<Checkpoint>| c.filter(|c| c.generation == target);
    let (cp, activate) = match st.active_generation {
        Some(a) if a == target => (of_target(st.checkpoint), false),
        // The manifest switched generations: the index switch is due (spec 05 5.3 step 3).
        Some(_) => (of_target(st.staged_checkpoint), true),
        // An interrupted re-index.
        None => (of_target(st.checkpoint), true),
    };
    let Some(cp) = cp else {
        return reindex(ReindexReason::NoCheckpoint);
    };
    // The CDX was truncated (repair, torn tail) since the index was written. `floor` makes this
    // hold even when new puts grew the CDX past the stale checkpoint before the first pass.
    let limit = floor.map_or(end, |f| f.min(end));
    if cp.cdx_offset > limit {
        return reindex(ReindexReason::BeyondCdx);
    }
    // Only at the first resolution after open, never later, so a staging in progress is never
    // discarded.
    let discard = if floor.is_some() {
        st.staged_checkpoint
            .filter(|s| s.generation > target)
            .map(|s| s.generation)
    } else {
        None
    };
    Plan::Resume {
        from: cp.cdx_offset,
        activate,
        discard,
    }
}

// ---- Test hooks.

/// Hooks that let tests hold the indexer back, kill it, or make its applies fail.
#[cfg(test)]
pub(crate) struct IndexHooks {
    /// When `Some`, the task acquires and forgets one permit at the start of each pass.
    pub(crate) hold: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Simulated kill: the task returns at the next check; nothing applied, flushed or acked.
    pub(crate) abandon: AtomicBool,
    /// Each apply consumes one and fails instead.
    pub(crate) fail_applies: std::sync::atomic::AtomicU32,
    /// Successful apply calls (passes, fallbacks, GC).
    pub(crate) applies: std::sync::atomic::AtomicU64,
    /// `Plan::Reindex` chosen.
    pub(crate) reindexes: std::sync::atomic::AtomicU64,
    /// Incremented each time `purge` returns 0.
    pub(crate) purge_idle: tokio::sync::watch::Sender<u64>,
    /// True while the task waits for a command with nothing left to do.
    pub(crate) parked: tokio::sync::watch::Sender<bool>,
}

#[cfg(test)]
impl Default for IndexHooks {
    fn default() -> Self {
        IndexHooks {
            hold: Mutex::new(None),
            abandon: AtomicBool::new(false),
            fail_applies: std::sync::atomic::AtomicU32::new(0),
            applies: std::sync::atomic::AtomicU64::new(0),
            reindexes: std::sync::atomic::AtomicU64::new(0),
            purge_idle: tokio::sync::watch::channel(0).0,
            parked: tokio::sync::watch::channel(false).0,
        }
    }
}

#[cfg(test)]
impl IndexHooks {
    async fn before_pass(&self) {
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

#[cfg(test)]
pub(crate) type Hooks = Arc<IndexHooks>;
#[cfg(not(test))]
pub(crate) type Hooks = ();

// ---- Blocking half of a pass.

#[derive(Debug, thiserror::Error)]
enum PassError {
    #[error("index: {0}")]
    Index(#[from] masstuffy_index::Error),
    #[error("cdx: {0}")]
    Cdx(#[from] masstuffy_collection::Error),
    #[error("indexer job panicked")]
    Panicked,
    #[error("internal: {0}")]
    Internal(&'static str),
}

/// One collection of a pass, captured by the task from its `Track`.
struct Job {
    key: CollectionKey,
    id: CollectionId,
    snap: Arc<Snapshot>,
    end: u64,
    cursor: Option<(u32, u64)>,
    active: bool,
    floor: Option<u64>,
    force: bool,
}

struct JobDone {
    /// Generation and offset after the last line applied.
    cursor: (u32, u64),
    /// The cursor's generation is the collection's active generation in the index.
    active: bool,
    /// The cursor reached the `end` captured at the start of the pass.
    caught_up: bool,
}

struct PassOutcome {
    jobs: Vec<(CollectionKey, Result<JobDone, PassError>)>,
    /// A structural batch (`DropCollection`, `ReplaceGeneration`, `DiscardStaged`) was applied.
    structural: bool,
    #[cfg(test)]
    abandoned: bool,
}

/// Applies a batch (the test hooks may fail it first).
fn apply(
    index: &dyn SecondaryIndex,
    ops: &[Op<'_>],
    hooks: &Hooks,
) -> Result<(), masstuffy_index::Error> {
    #[cfg(test)]
    {
        use std::sync::atomic::Ordering::AcqRel;
        let mut n = hooks.fail_applies.load(Ordering::Acquire);
        while n > 0 {
            match hooks
                .fail_applies
                .compare_exchange(n, n - 1, AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Err(masstuffy_index::Error::Backend(Box::new(
                        std::io::Error::other("injected"),
                    )));
                }
                Err(cur) => n = cur,
            }
        }
    }
    index.apply(ops)?;
    #[cfg(test)]
    hooks.applies.fetch_add(1, Ordering::AcqRel);
    #[cfg(not(test))]
    let _ = hooks;
    Ok(())
}

fn is_structural(op: &Op<'_>) -> bool {
    matches!(
        op,
        Op::DropCollection { .. } | Op::ReplaceGeneration { .. } | Op::DiscardStaged { .. }
    )
}

/// `Stage` (`from` is `Some`) or `Unstage` (`None`): applied only while the collection's active
/// generation is below `generation`. Returns whether a batch was applied.
fn stage_blocking(
    index: &dyn SecondaryIndex,
    key: CollectionKey,
    generation: u32,
    from: Option<u64>,
    hooks: &Hooks,
) -> Result<bool, masstuffy_index::Error> {
    let state = index.state(key)?;
    let below = state
        .and_then(|s| s.active_generation)
        .is_some_and(|a| a < generation);
    if !below {
        return Ok(false);
    }
    // A staged checkpoint of another generation belongs to an earlier rebuild whose index switch
    // is still pending (after an index failure): keep it, or the indexer would have to re-index
    // the active generation. The new generation then re-indexes after its own switch.
    if from.is_some()
        && state
            .and_then(|s| s.staged_checkpoint)
            .is_some_and(|c| c.generation != generation)
    {
        return Ok(false);
    }
    let mut ops = Vec::with_capacity(2);
    ops.push(Op::DiscardStaged {
        collection: key,
        generation,
    });
    if let Some(cdx_offset) = from {
        ops.push(Op::Checkpoint {
            collection: key,
            checkpoint: Checkpoint {
                generation,
                cdx_offset,
            },
        });
    }
    apply(index, &ops, hooks)?;
    Ok(true)
}

/// Orphan GC: drops every index collection that is neither tracked nor kept. Returns whether a
/// structural batch was applied.
fn gc_blocking(index: &dyn SecondaryIndex, known: &HashSet<CollectionKey>, hooks: &Hooks) -> bool {
    let run = || -> Result<bool, masstuffy_index::Error> {
        let ops: Vec<Op<'static>> = index
            .collections()?
            .into_iter()
            .filter(|k| !known.contains(k))
            .map(|collection| Op::DropCollection { collection })
            .collect();
        if ops.is_empty() {
            return Ok(false);
        }
        info!(orphans = ops.len(), "dropping orphan index collections");
        apply(index, &ops, hooks)?;
        Ok(true)
    };
    match run() {
        Ok(applied) => applied,
        Err(e) => {
            warn!("index orphan GC failed: {e}");
            false
        }
    }
}

/// Builds the ops of one job into `ops` (at most `share` CDX lines). On error the caller truncates
/// `ops` back to its start.
fn build_job(
    index: &dyn SecondaryIndex,
    job: &Job,
    share: usize,
    ops: &mut Vec<Op<'static>>,
    hooks: &Hooks,
) -> Result<(JobDone, bool), PassError> {
    use masstuffy_collection::Error as C;
    #[cfg(not(test))]
    let _ = hooks;
    let start = ops.len();
    let target = job.snap.generation();
    let mut plan = match job.cursor {
        Some((g, off)) if g == target && !job.force => Plan::Resume {
            from: off,
            activate: !job.active,
            discard: None,
        },
        _ => {
            let state = index.state(job.key)?;
            resolve(state, target, job.end, job.floor, job.force)
        }
    };
    let (mut lines, activate) = loop {
        let (from, activate) = match plan {
            Plan::Resume {
                from,
                activate,
                discard,
            } => {
                if let Some(generation) = discard {
                    ops.push(Op::DiscardStaged {
                        collection: job.key,
                        generation,
                    });
                }
                (Some(from), activate)
            }
            Plan::Reindex { drop, reason } => {
                #[cfg(test)]
                hooks.reindexes.fetch_add(1, Ordering::AcqRel);
                info!(collection = %job.id, ?reason, "re-indexing collection");
                if drop {
                    ops.push(Op::DropCollection {
                        collection: job.key,
                    });
                }
                (None, true)
            }
        };
        match job.snap.cdx_lines(from, job.end) {
            Ok(lines) => break (lines, activate),
            // A resume point that is not a line start of this CDX (or lies beyond it): the index
            // does not describe this file. Start again from the header.
            Err(C::CdxRange { .. } | C::CdxTruncated { .. }) if from.is_some() => {
                ops.truncate(start);
                plan = Plan::Reindex {
                    drop: true,
                    reason: ReindexReason::Misaligned,
                };
            }
            Err(e) => return Err(e.into()),
        }
    };
    let reindex = matches!(plan, Plan::Reindex { .. });
    let mut read = 0usize;
    while read < share {
        let Some(line) = lines.next_line()? else {
            break;
        };
        match line.entry.kind {
            Kind::Add => ops.push(Op::Insert {
                collection: job.key,
                generation: target,
                entry: line.entry.into_owned(),
            }),
            Kind::Tombstone => ops.push(Op::Tombstone {
                collection: job.key,
                generation: target,
                position: Position {
                    segment: line.entry.segment,
                    offset: line.entry.offset,
                },
            }),
        }
        read += 1;
    }
    let pos = lines.position();
    if read > 0 || reindex || ops.len() > start {
        ops.push(Op::Checkpoint {
            collection: job.key,
            checkpoint: Checkpoint {
                generation: target,
                cdx_offset: pos,
            },
        });
    }
    let caught_up = pos == job.end;
    let activated = caught_up && activate;
    if activated {
        ops.push(Op::ReplaceGeneration {
            collection: job.key,
            generation: target,
        });
    }
    let structural = ops
        .get(start..)
        .is_some_and(|s| s.iter().any(is_structural));
    Ok((
        JobDone {
            cursor: (target, pos),
            active: !activate || activated,
            caught_up,
        },
        structural,
    ))
}

/// Blocking half of one pass: orphan GC, then one batch for every job.
fn pass_blocking(
    index: &dyn SecondaryIndex,
    jobs: &[Job],
    share: usize,
    gc: Option<&HashSet<CollectionKey>>,
    hooks: &Hooks,
) -> PassOutcome {
    let mut structural = false;
    if let Some(known) = gc {
        structural |= gc_blocking(index, known, hooks);
    }
    let mut ops: Vec<Op<'static>> = Vec::with_capacity(
        share
            .saturating_mul(jobs.len())
            .saturating_add(4usize.saturating_mul(jobs.len())),
    );
    struct Built {
        job: usize,
        range: Range<usize>,
        done: JobDone,
        structural: bool,
    }
    let mut results: Vec<Option<Result<JobDone, PassError>>> = jobs.iter().map(|_| None).collect();
    let mut built: Vec<Built> = Vec::new();
    for (i, job) in jobs.iter().enumerate() {
        let start = ops.len();
        match build_job(index, job, share, &mut ops, hooks) {
            Ok((done, st)) if ops.len() > start => built.push(Built {
                job: i,
                range: start..ops.len(),
                done,
                structural: st,
            }),
            Ok((done, _)) => set(&mut results, i, Ok(done)),
            Err(e) => {
                ops.truncate(start);
                set(&mut results, i, Err(e));
            }
        }
    }
    if !built.is_empty() {
        #[cfg(test)]
        if hooks.abandon.load(Ordering::Acquire) {
            return PassOutcome {
                jobs: Vec::new(),
                structural,
                abandoned: true,
            };
        }
        let apply_range = |r: &Range<usize>| -> Result<(), PassError> {
            let slice = ops.get(r.clone()).ok_or(PassError::Internal("op range"))?;
            apply(index, slice, hooks).map_err(PassError::from)
        };
        match apply_range(&(0..ops.len())) {
            Ok(()) => {
                for b in built {
                    structural |= b.structural;
                    set(&mut results, b.job, Ok(b.done));
                }
            }
            Err(e) if built.len() > 1 => {
                // One bad collection must not block the others: apply each job alone.
                warn!(
                    "index batch of {} collections failed ({e}); retrying one by one",
                    built.len()
                );
                for b in built {
                    match apply_range(&b.range) {
                        Ok(()) => {
                            structural |= b.structural;
                            set(&mut results, b.job, Ok(b.done));
                        }
                        Err(e) => set(&mut results, b.job, Err(e)),
                    }
                }
            }
            Err(e) => {
                if let Some(b) = built.first() {
                    set(&mut results, b.job, Err(e));
                }
            }
        }
    }
    let jobs_out = jobs
        .iter()
        .zip(results)
        .map(|(j, r)| {
            (
                j.key,
                r.unwrap_or(Err(PassError::Internal("no job result"))),
            )
        })
        .collect();
    PassOutcome {
        jobs: jobs_out,
        structural,
        #[cfg(test)]
        abandoned: false,
    }
}

fn set(
    results: &mut [Option<Result<JobDone, PassError>>],
    i: usize,
    r: Result<JobDone, PassError>,
) {
    if let Some(slot) = results.get_mut(i) {
        *slot = Some(r);
    }
}

// ---- The task.

pub(crate) struct IndexerParams {
    pub(crate) rx: mpsc::Receiver<IndexCmd>,
    pub(crate) hub: Arc<Hub>,
    pub(crate) backend: IndexBackend,
    /// Every collection loaded at open.
    pub(crate) initial: Vec<Arc<IndexFeed>>,
    /// Collections skipped at open: never dropped by GC.
    pub(crate) keep: HashSet<CollectionKey>,
    pub(crate) batch_ops: usize,
    /// Every blocking job gets a clone, so the directory stays locked until the task exits.
    pub(crate) lock: Arc<DirLock>,
    #[cfg(test)]
    pub(crate) hooks: Hooks,
}

/// Spawns the indexer task.
pub(crate) fn spawn_indexer(p: IndexerParams) -> JoinHandle<()> {
    let tracks = p
        .initial
        .into_iter()
        .map(|f| (f.key, Track::new(f)))
        .collect();
    let ix = Indexer {
        rx: p.rx,
        hub: p.hub,
        backend: Arc::new(p.backend),
        tracks,
        keep: p.keep,
        batch_ops: p.batch_ops.max(1),
        lock: p.lock,
        #[cfg(test)]
        hooks: p.hooks,
        pending: Vec::new(),
        admin_ops: VecDeque::new(),
        gc_pending: true,
        purge_pending: true,
        stop: Vec::new(),
        stopping: false,
        closed: false,
    };
    tokio::spawn(ix.run())
}

/// Indexer-owned per-collection state (no lock: only the indexer task touches it).
struct Track {
    feed: Arc<IndexFeed>,
    /// In-memory resume point (generation, offset after the last applied line). `None`: resolve
    /// from `index.state()`.
    cursor: Option<(u32, u64)>,
    /// The cursor's generation is the collection's active generation in the index.
    active: bool,
    /// `Some(initial_len)` until the first successful resolution, then `None` (rule 6).
    floor: Option<u64>,
    /// The next resolution must re-index.
    force: bool,
    /// Incremented on every failure of this track (barriers compare epochs).
    fail_epoch: u64,
    /// The last attempt failed and no apply has succeeded since (reset on success, kept by a
    /// forced re-index): the next barrier counts as that track's retry.
    failed: bool,
}

impl Track {
    fn new(feed: Arc<IndexFeed>) -> Track {
        Track {
            floor: Some(feed.initial_len),
            feed,
            cursor: None,
            active: false,
            force: false,
            fail_epoch: 0,
            failed: false,
        }
    }
}

struct Target {
    key: CollectionKey,
    generation: u32,
    len: u64,
    /// `fail_epoch` of the track when last looked at.
    epoch: u64,
    /// The track was requeued after a failure on behalf of this barrier: a second failure fails
    /// the barrier.
    retried: bool,
}

struct Pending {
    targets: Vec<Target>,
    reply: oneshot::Sender<Result<(), Error>>,
}

enum Status {
    Wait,
    Done,
    Failed(Vec<CollectionId>),
}

/// Field order is load-bearing: `backend` (the index handle) is declared before `lock`, so it
/// drops first and the redb file is closed before the directory lock can be released. `run`
/// also drops them explicitly in that order on its normal exit.
struct Indexer {
    rx: mpsc::Receiver<IndexCmd>,
    hub: Arc<Hub>,
    backend: Arc<IndexBackend>,
    tracks: HashMap<CollectionKey, Track>,
    keep: HashSet<CollectionKey>,
    batch_ops: usize,
    lock: Arc<DirLock>,
    #[cfg(test)]
    hooks: Hooks,
    pending: Vec<Pending>,
    /// `Stage` / `Unstage` commands, one handled per loop turn.
    admin_ops: VecDeque<AdminOp>,
    gc_pending: bool,
    purge_pending: bool,
    stop: Vec<oneshot::Sender<()>>,
    stopping: bool,
    closed: bool,
}

impl Indexer {
    async fn run(mut self) {
        loop {
            // 1. Commands, without waiting.
            loop {
                match self.rx.try_recv() {
                    Ok(c) => self.handle(c),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.closed = true;
                        break;
                    }
                }
            }
            #[cfg(test)]
            if self.hooks.abandon.load(Ordering::Acquire) {
                return; // Simulated kill: no apply, no flush, no ack.
            }
            // 2. Work. Staging commands first: a pass must not index the new generation before
            // its staged checkpoint is in place.
            if !self.admin_ops.is_empty() {
                self.admin_step().await;
                self.check_pending();
                continue;
            }
            if self.gc_pending || !self.hub.is_empty() {
                if !self.pass().await {
                    return;
                }
                self.check_pending();
                continue;
            }
            self.check_pending();
            if self.stopping || self.closed {
                break;
            }
            if self.purge_pending {
                self.purge_step().await;
                continue;
            }
            #[cfg(test)]
            self.hooks.parked.send_replace(true);
            let next = self.rx.recv().await;
            #[cfg(test)]
            self.hooks.parked.send_replace(false);
            match next {
                Some(c) => self.handle(c),
                None => self.closed = true,
            }
        }
        self.finish().await;
        // Index handle first, then the directory lock (see the field-order note on `Indexer`).
        let Indexer { backend, lock, .. } = self;
        drop(backend);
        drop(lock);
    }

    async fn finish(&mut self) {
        let backend = Arc::clone(&self.backend);
        let lock = Arc::clone(&self.lock);
        match tokio::task::spawn_blocking(move || {
            let _lock = lock;
            let backend = backend; // dropped before the lock
            backend.flush()
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("secondary index flush failed: {e}"),
            Err(e) => error!("secondary index flush task failed: {e}"),
        }
        for p in self.pending.drain(..) {
            let _ = p.reply.send(Err(Error::IndexUnavailable));
        }
        for ack in self.stop.drain(..) {
            let _ = ack.send(());
        }
    }

    fn handle(&mut self, cmd: IndexCmd) {
        match cmd {
            IndexCmd::Wake => {}
            IndexCmd::Barrier(r) => self.add_pending(r),
            IndexCmd::Reindex(r) => {
                self.absorb_hub();
                for t in self.tracks.values_mut() {
                    t.cursor = None;
                    t.active = false;
                    t.force = true;
                    self.hub.enqueue(&t.feed);
                }
                self.gc_pending = true;
                self.add_pending(r);
            }
            IndexCmd::Stage {
                key,
                generation,
                from,
                reply,
            } => self.admin_ops.push_back(AdminOp::Stage {
                key,
                generation,
                from,
                reply,
            }),
            IndexCmd::Unstage {
                key,
                generation,
                reply,
            } => self.admin_ops.push_back(AdminOp::Unstage {
                key,
                generation,
                reply,
            }),
            IndexCmd::Forget { key, reply } => {
                self.admin_ops.push_back(AdminOp::Forget { key, reply });
            }
            IndexCmd::AwaitActive {
                key,
                generation,
                reply,
            } => self.add_pending_one(key, generation, reply),
            IndexCmd::Shutdown(ack) => {
                self.stopping = true;
                self.stop.push(ack);
            }
        }
    }

    /// A barrier on one collection, for a generation that is (or will be) the snapshot's.
    fn add_pending_one(
        &mut self,
        key: CollectionKey,
        generation: u32,
        reply: oneshot::Sender<Result<(), Error>>,
    ) {
        self.absorb_hub();
        let Some(t) = self.tracks.get(&key) else {
            let _ = reply.send(Err(Error::IndexUnavailable));
            return;
        };
        // A track whose last attempt failed gets its retry now.
        let failed = t.cursor.is_none() && t.failed;
        if failed {
            self.hub.enqueue(&t.feed);
        }
        let (snap, len) = t.feed.published.tail();
        self.pending.push(Pending {
            targets: vec![Target {
                key,
                generation: generation.max(snap.generation()),
                len,
                epoch: t.fail_epoch,
                retried: failed,
            }],
            reply,
        });
    }

    /// `Forget`: drops the hub entry and the track, then `DropCollection` in the blocking pool.
    async fn forget_step(&mut self, key: CollectionKey, reply: oneshot::Sender<Result<(), Error>>) {
        // Before the blocking job: no later pass may recreate the track.
        self.hub.remove(key);
        self.tracks.remove(&key);
        let index = Arc::clone(&self.backend.index);
        let lock = Arc::clone(&self.lock);
        #[cfg(test)]
        let hooks = Arc::clone(&self.hooks);
        #[cfg(not(test))]
        let hooks = ();
        let joined = tokio::task::spawn_blocking(move || {
            let _lock = lock;
            let index = index; // dropped before the lock
            apply(&*index, &[Op::DropCollection { collection: key }], &hooks)
        })
        .await;
        let res = match joined {
            Ok(Ok(())) => {
                self.purge_pending = true;
                Ok(())
            }
            Ok(Err(e)) => {
                self.gc_pending = true;
                Err(Error::IndexQuery(e))
            }
            Err(e) => {
                error!("index forget task failed: {e}");
                self.gc_pending = true;
                Err(Error::IndexUnavailable)
            }
        };
        let _ = reply.send(res);
    }

    /// Runs one `Stage` / `Unstage` / `Forget` in the blocking pool and replies.
    async fn admin_step(&mut self) {
        let Some(op) = self.admin_ops.pop_front() else {
            return;
        };
        let op = match op {
            AdminOp::Forget { key, reply } => return self.forget_step(key, reply).await,
            op => op,
        };
        let index = Arc::clone(&self.backend.index);
        let lock = Arc::clone(&self.lock);
        #[cfg(test)]
        let hooks = Arc::clone(&self.hooks);
        #[cfg(not(test))]
        let hooks = ();
        let (key, generation, from) = match &op {
            AdminOp::Stage {
                key,
                generation,
                from,
                ..
            } => (*key, *generation, Some(*from)),
            AdminOp::Unstage {
                key, generation, ..
            } => (*key, *generation, None),
            AdminOp::Forget { .. } => return,
        };
        let joined = tokio::task::spawn_blocking(move || {
            let _lock = lock;
            let index = index; // dropped before the lock
            stage_blocking(&*index, key, generation, from, &hooks)
        })
        .await;
        let res = match joined {
            Ok(Ok(applied)) => Ok(applied),
            Ok(Err(e)) => Err(Error::IndexQuery(e)),
            Err(e) => {
                error!("index staging task failed: {e}");
                Err(Error::IndexUnavailable)
            }
        };
        if matches!(res, Ok(true)) {
            self.purge_pending = true;
        }
        match op {
            AdminOp::Stage { reply, .. } => {
                let _ = reply.send(res);
            }
            AdminOp::Unstage { reply, .. } => {
                let _ = reply.send(res.map(|_| ()));
            }
            AdminOp::Forget { .. } => {}
        }
    }

    /// Creates the track of every queued feed that has none yet, so that a barrier covers a
    /// collection created after the task started.
    fn absorb_hub(&mut self) {
        for f in self.hub.queued() {
            self.tracks
                .entry(f.key)
                .or_insert_with(|| Track::new(Arc::clone(&f)));
        }
    }

    fn add_pending(&mut self, reply: oneshot::Sender<Result<(), Error>>) {
        self.absorb_hub();
        let mut targets = Vec::with_capacity(self.tracks.len());
        for (key, t) in &self.tracks {
            // A track whose last attempt failed gets its retry now.
            let failed = t.cursor.is_none() && t.failed;
            if failed {
                self.hub.enqueue(&t.feed);
            }
            let (snap, len) = t.feed.published.tail();
            targets.push(Target {
                key: *key,
                generation: snap.generation(),
                len,
                epoch: t.fail_epoch,
                retried: failed,
            });
        }
        self.pending.push(Pending { targets, reply });
    }

    fn check_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut keep = Vec::with_capacity(self.pending.len());
        for mut p in std::mem::take(&mut self.pending) {
            match self.status(&mut p) {
                Status::Wait => keep.push(p),
                Status::Done => {
                    let _ = p.reply.send(Ok(()));
                }
                Status::Failed(collections) => {
                    let _ = p.reply.send(Err(Error::IndexLagging { collections }));
                }
            }
        }
        self.pending = keep;
    }

    fn status(&self, p: &mut Pending) -> Status {
        if self.gc_pending {
            return Status::Wait;
        }
        let mut waiting = false;
        let mut failed = Vec::new();
        for t in &mut p.targets {
            let Some(track) = self.tracks.get(&t.key) else {
                continue;
            };
            let satisfied = track.cursor.is_some_and(|(g, off)| {
                (g == t.generation && off >= t.len && track.active) || g > t.generation
            });
            if satisfied {
                continue;
            }
            if track.cursor.is_none() && track.fail_epoch > t.epoch {
                t.epoch = track.fail_epoch;
                if t.retried {
                    failed.push(track.feed.id);
                } else {
                    t.retried = true;
                    self.hub.enqueue(&track.feed);
                    waiting = true;
                }
            } else {
                waiting = true;
            }
        }
        if !failed.is_empty() {
            Status::Failed(failed)
        } else if waiting {
            Status::Wait
        } else {
            Status::Done
        }
    }

    /// One pass: pops the dirty feeds and applies one batch for all of them. Returns false if the
    /// task must stop at once (test hook).
    async fn pass(&mut self) -> bool {
        #[cfg(test)]
        self.hooks.before_pass().await;
        let mut jobs = Vec::new();
        while jobs.len() < MAX_JOBS_PER_PASS {
            let Some(feed) = self.hub.pop_feed() else {
                break;
            };
            let (snap, end) = feed.published.tail();
            let t = self
                .tracks
                .entry(feed.key)
                .or_insert_with(|| Track::new(Arc::clone(&feed)));
            jobs.push(Job {
                key: feed.key,
                id: feed.id,
                snap,
                end,
                cursor: t.cursor,
                active: t.active,
                floor: t.floor,
                force: t.force,
            });
        }
        let gc = self.gc_pending.then(|| {
            self.tracks
                .keys()
                .chain(self.keep.iter())
                .copied()
                .collect::<HashSet<_>>()
        });
        let gc_requested = gc.is_some();
        let share = (self.batch_ops / jobs.len().max(1)).max(1);
        let index = Arc::clone(&self.backend.index);
        let lock = Arc::clone(&self.lock);
        #[cfg(test)]
        let hooks = Arc::clone(&self.hooks);
        #[cfg(not(test))]
        let hooks = ();
        let keys: Vec<(CollectionKey, CollectionId)> = jobs.iter().map(|j| (j.key, j.id)).collect();
        let joined = tokio::task::spawn_blocking(move || {
            // Declared in this order so that the index handle is dropped before the lock.
            let _lock = lock;
            let index = index;
            pass_blocking(&*index, &jobs, share, gc.as_ref(), &hooks)
        })
        .await;
        let outcome = match joined {
            Ok(o) => o,
            Err(e) => {
                error!("indexer pass panicked: {e}");
                PassOutcome {
                    jobs: keys
                        .into_iter()
                        .map(|(k, _)| (k, Err(PassError::Panicked)))
                        .collect(),
                    structural: false,
                    #[cfg(test)]
                    abandoned: false,
                }
            }
        };
        #[cfg(test)]
        if outcome.abandoned {
            return false;
        }
        if gc_requested {
            self.gc_pending = false;
        }
        if outcome.structural {
            self.purge_pending = true;
        }
        for (key, res) in outcome.jobs {
            let Some(t) = self.tracks.get_mut(&key) else {
                continue;
            };
            match res {
                Ok(done) => {
                    t.cursor = Some(done.cursor);
                    t.active = done.active;
                    t.floor = None;
                    t.force = false;
                    t.failed = false;
                    if !done.caught_up {
                        self.hub.enqueue(&t.feed);
                    }
                    debug!(collection = %t.feed.id, cursor = ?done.cursor, active = done.active, "indexed");
                }
                Err(e) => {
                    // Not requeued: it retries at the next wake or barrier. The next pass reads
                    // the index state again and resumes from the durable checkpoint.
                    t.cursor = None;
                    t.active = false;
                    t.fail_epoch = t.fail_epoch.saturating_add(1);
                    t.failed = true;
                    error!(collection = %t.feed.id, "indexing failed: {e}");
                }
            }
        }
        true
    }

    /// Lowest-priority work: reclaim dead rows left by structural batches, one bounded step at a
    /// time. The loop looks at commands and the hub between steps.
    async fn purge_step(&mut self) {
        let index = Arc::clone(&self.backend.index);
        let lock = Arc::clone(&self.lock);
        let r = tokio::task::spawn_blocking(move || {
            let _lock = lock;
            let index = index; // dropped before the lock
            index.purge(PURGE_STEP)
        })
        .await;
        match r {
            Ok(Ok(0)) => {
                self.purge_pending = false;
                #[cfg(test)]
                self.hooks.purge_idle.send_modify(|n| *n += 1);
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!("index purge failed: {e}");
                self.purge_pending = false;
            }
            Err(e) => {
                warn!("index purge task failed: {e}");
                self.purge_pending = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(generation: u32, cdx_offset: u64) -> Checkpoint {
        Checkpoint {
            generation,
            cdx_offset,
        }
    }

    fn st(active: Option<u32>, c: Option<Checkpoint>, s: Option<Checkpoint>) -> CollectionState {
        CollectionState {
            active_generation: active,
            checkpoint: c,
            staged_checkpoint: s,
        }
    }

    fn re(drop: bool, reason: ReindexReason) -> Plan {
        Plan::Reindex { drop, reason }
    }

    #[test]
    fn resolve_rules() {
        let t = 2;
        // 1. unknown collection.
        assert_eq!(
            resolve(None, t, 200, None, false),
            re(false, ReindexReason::Unknown)
        );
        // 2. forced.
        let ok = st(Some(t), Some(cp(t, 100)), None);
        assert_eq!(
            resolve(Some(ok), t, 200, None, true),
            re(true, ReindexReason::Forced)
        );
        // 3. index ahead.
        assert_eq!(
            resolve(
                Some(st(Some(3), Some(cp(3, 10)), None)),
                t,
                200,
                None,
                false
            ),
            re(true, ReindexReason::IndexAhead)
        );
        // 4. active == target.
        assert_eq!(
            resolve(Some(ok), t, 200, None, false),
            Plan::Resume {
                from: 100,
                activate: false,
                discard: None
            }
        );
        assert_eq!(
            resolve(
                Some(st(Some(t), Some(cp(1, 100)), None)),
                t,
                200,
                None,
                false
            ),
            re(true, ReindexReason::NoCheckpoint)
        );
        // active < target.
        assert_eq!(
            resolve(
                Some(st(Some(1), Some(cp(1, 900)), Some(cp(2, 50)))),
                t,
                200,
                None,
                false
            ),
            Plan::Resume {
                from: 50,
                activate: true,
                discard: None
            }
        );
        assert_eq!(
            resolve(
                Some(st(Some(1), Some(cp(1, 900)), None)),
                t,
                200,
                None,
                false
            ),
            re(true, ReindexReason::NoCheckpoint)
        );
        // no active generation.
        assert_eq!(
            resolve(Some(st(None, Some(cp(t, 80)), None)), t, 200, None, false),
            Plan::Resume {
                from: 80,
                activate: true,
                discard: None
            }
        );
        // 6. beyond the CDX, or beyond the floor.
        assert_eq!(
            resolve(
                Some(st(Some(t), Some(cp(t, 300)), None)),
                t,
                200,
                None,
                false
            ),
            re(true, ReindexReason::BeyondCdx)
        );
        assert_eq!(
            resolve(
                Some(st(Some(t), Some(cp(t, 150)), None)),
                t,
                200,
                Some(120),
                false
            ),
            re(true, ReindexReason::BeyondCdx)
        );
        // 7. stale staged generation: discarded at the first resolution only.
        let staged = st(Some(t), Some(cp(t, 100)), Some(cp(t + 1, 10)));
        assert_eq!(
            resolve(Some(staged), t, 200, Some(200), false),
            Plan::Resume {
                from: 100,
                activate: false,
                discard: Some(t + 1)
            }
        );
        assert_eq!(
            resolve(Some(staged), t, 200, None, false),
            Plan::Resume {
                from: 100,
                activate: false,
                discard: None
            }
        );
    }
}
