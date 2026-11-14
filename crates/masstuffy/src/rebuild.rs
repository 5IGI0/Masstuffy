//! Rebuild of a collection into a new generation (spec 04 4.5, spec 05 5.3), orchestrated while
//! puts and reads continue.
//!
//! # Protocol
//!
//! [`CollectionHandle::rebuild`] registers the rebuild (`RebuildSlot::active`, under the gate read
//! guard) and runs it as a spawned unit, so that dropping the caller's future cannot leave it half
//! done:
//!
//! 1. take a `rebuild_sem` permit, then the collection's admin mutex (held to the end, on purpose:
//!    exclusivity with `rebuild_index`, shutdown and the other admin operations; the writer task
//!    never takes it);
//! 2. **start** (blocking, `RebuildSlot::rb` locked): `Rebuilder::start` on the current snapshot.
//!    Holding the slot mutex makes `start` and every writer reopen mutually exclusive, so
//!    `Collection::open` can never delete the files of a `Rebuilder` that survives it;
//! 3. **stage**: `IndexCmd::Stage` clears any staged rows of the new generation and puts a staged
//!    checkpoint at the start of the new CDX, so the indexer resumes there after the switch;
//! 4. **copy rounds**: `copy_until(published CDX length)` until the remaining delta is below 1 MiB
//!    of CDX or 8 rounds ran (puts keep committing to the old generation);
//! 5. **finish**: `Msg::Finish` makes the writer task stop draining, run `Rebuilder::finish` (final
//!    delta, fsyncs, checkpoint, the manifest swap: the single commit point) and publish the new
//!    snapshot; the old one becomes the *retired* snapshot, so locators of the old generation stay
//!    readable;
//! 6. **index switch**: `IndexCmd::AwaitActive` returns once the indexer has indexed the new CDX
//!    and activated the generation (`ReplaceGeneration`), after which the retired snapshot is
//!    cleared (old-generation locators now give `StaleLocator`) and the old files are removed.
//!
//! Any failure before the commit aborts the `Rebuilder` (its files are deleted) and unstages the
//! index; the old generation stays active. Requests queued for the old generation when the switch
//! happens are answered `Rejected::Stale` by the writer task and prepared again by their caller.
//!
//! # Crash behaviour
//!
//! The commit point is the manifest swap. A crash before it leaves generation `N+1` files that
//! `Collection::open` deletes (the index's staged rows are discarded at the first resolution); a
//! crash after it leaves both generations on disk, `open` deletes the old one and the indexer
//! resumes at the staged checkpoint (or re-indexes if staging never happened).
//!
//! # Cancellation
//!
//! Shutdown is the cancel: it sets `closing` and every `RebuildSlot::cancel`, the unit observes
//! them at each phase and its waits (index replies, test pauses) are raced with the signal. After
//! `Finish` was sent the final step is bounded (what 8 rounds left) but still cancellable: its
//! final `copy_until` checks the cancel flag, so a shutdown (or a writer reopen) fails it with
//! `RebuildCancelled`; nothing is committed, the new generation's files are removed by the
//! writer task, the index is unstaged and the unit returns `ShuttingDown` (or `Rebuild` with the
//! conflict error for a reopen).
//!
//! If the index wait after the commit fails (index failure, shutdown), the rebuild still
//! succeeds with `index_switched = false`: old-generation locators and index hits return
//! `StaleLocator` until the index catches up.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;

use masstuffy_collection::{
    Codec, CollectionId, MAX_REBUILD_WORKERS, RebuildOptions as CollectionOptions,
    RebuildReport as CopyReport, Rebuilder, Unreadable,
};
use masstuffy_index::CollectionKey;
use tokio::sync::oneshot;
use tracing::warn;

use crate::collection::{CollectionHandle, Shared};
use crate::error::Error;
use crate::indexer::IndexCmd;
use crate::repository::Core;
use crate::writer::{FinishDone, FinishFail, Msg, RebuildPoint};

/// Copy rounds stop when less than this much CDX is left (bytes).
const CATCHUP_BYTES: u64 = 1 << 20;
/// Most copy rounds before the final step.
const MAX_ROUNDS: u32 = 8;

/// Per-collection rebuild state, shared by the rebuild unit and the writer task (`Arc`).
pub(crate) struct RebuildSlot {
    /// The running `Rebuilder`, if any. Std mutex, only locked in blocking jobs, never across an
    /// `.await`, never while another lock is held. Held by the rebuild unit for `start` and for
    /// each `copy_until`; by the writer task while it takes the `Rebuilder` for `finish`, and for
    /// the whole of a reopen (retire + `Collection::open`): this makes `start` and every reopen
    /// mutually exclusive, so `Collection::open` can never delete the files of a `Rebuilder` that
    /// survives it.
    pub(crate) rb: Mutex<Option<Rebuilder>>,
    /// Cancel flag given to every `Rebuilder` (`set_cancel`). Set (SeqCst) by a writer reopen and
    /// by shutdown; reset to false only by the start job while it holds `rb`.
    pub(crate) cancel: Arc<AtomicBool>,
    /// A rebuild is registered (from the `rebuild` call until its unit ends).
    pub(crate) active: AtomicBool,
}

impl RebuildSlot {
    pub(crate) fn new() -> RebuildSlot {
        RebuildSlot {
            rb: Mutex::new(None),
            cancel: Arc::new(AtomicBool::new(false)),
            active: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Rebuilder>> {
        // A poisoned lock is recovered: the guarded value is an `Option` that a panicking job
        // leaves either intact or taken.
        self.rb.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Clears `RebuildSlot::active` when the unit ends (also on panic or unwind).
struct ActiveGuard(Arc<RebuildSlot>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::SeqCst);
    }
}

/// The result of [`race`].
pub(crate) enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Polls both futures; returns the first that completes (left first on a tie). The other is
/// dropped. (The library has no `tokio::select!`: the `macros` feature is not enabled.)
pub(crate) async fn race<A: Future, B: Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let mut a = pin!(a);
    let mut b = pin!(b);
    std::future::poll_fn(|cx| {
        if let Poll::Ready(v) = a.as_mut().poll(cx) {
            return Poll::Ready(Either::Left(v));
        }
        if let Poll::Ready(v) = b.as_mut().poll(cx) {
            return Poll::Ready(Either::Right(v));
        }
        Poll::Pending
    })
    .await
}

/// Options of [`CollectionHandle::rebuild`]. `None` fields take the repository config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RebuildOptions {
    /// Transcode even when the stored bytes could be copied (same dictionary), e.g. to apply a new
    /// level. Without it a level change on the same dictionary only affects new puts.
    pub recompress: bool,
    /// Damaged records: `Fail` (default, nothing committed) or `Skip` (reported).
    pub on_unreadable: Unreadable,
    /// Transcoding threads (`1..=256`); default `Config::rebuild_workers`.
    pub workers: Option<NonZeroUsize>,
    /// In-flight byte budget of the pool (non-zero); default `Config::rebuild_inflight_bytes`.
    pub max_inflight_bytes: Option<u64>,
}

/// What a rebuild did.
#[derive(Debug, Clone)]
pub struct RebuildReport {
    /// The collection-level report (generations, codec, counts, skipped records).
    pub copy: CopyReport,
    /// `copy_until` rounds run before the final step.
    pub rounds: u32,
    /// The secondary index activated the new generation before the old files were removed (false:
    /// the index lagged or was unavailable; it switches later on its own, and hits of the old
    /// generation give `StaleLocator` meanwhile).
    pub index_switched: bool,
    /// Old-generation files removed; `None` if the removal failed (logged; the next open removes
    /// them).
    pub old_files_removed: Option<usize>,
}

/// What the start job learned from the new `Rebuilder`.
struct StartInfo {
    generation: u32,
    cdx_start: u64,
    frontier: u64,
}

/// Why a copy round did not run to its end.
enum CopyErr {
    /// The slot was empty (a reopen took the `Rebuilder`).
    Gone,
    Failed(masstuffy_collection::Error),
}

/// A rebuild failure that happened before the commit.
fn rebuild_error(collection: CollectionId, source: masstuffy_collection::Error) -> Error {
    Error::Rebuild {
        collection,
        committed: false,
        source: Box::new(source),
    }
}

impl CollectionHandle {
    /// Rebuilds the collection into a new generation with `target` (spec 04 4.5). Puts and reads
    /// continue; see the module docs for the protocol, crash behaviour and errors.
    ///
    /// Runs as a spawned unit: dropping the returned future does not stop it. Locators of the
    /// replaced generation stay readable until the secondary index has switched to the new one,
    /// then give `StaleLocator`; so do `scan` pages of the old generation.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `InvalidOptions`, `DictNotFound`, `RebuildInProgress`, `RebuildAborted`,
    /// `Rebuild` (`committed` says whether the new generation is active), `CollectionUnavailable`,
    /// `Join`.
    pub async fn rebuild(
        &self,
        target: Codec,
        opts: RebuildOptions,
    ) -> Result<RebuildReport, Error> {
        let core = &self.core;
        if opts.workers.is_some_and(|w| w.get() > MAX_REBUILD_WORKERS) {
            return Err(Error::InvalidOptions("workers"));
        }
        if opts.max_inflight_bytes == Some(0) {
            return Err(Error::InvalidOptions("max_inflight_bytes"));
        }
        if let Codec::Zstd { dict_id, .. } = target
            && !core.resolver.0.contains(dict_id.get())
        {
            return Err(Error::DictNotFound(dict_id.get()));
        }
        // Registered under the gate read guard: a shutdown that starts later sets `closing` and
        // `cancel` after this point, so the unit cannot miss it.
        let guard = {
            let gate = core.gate.read().await;
            if *gate {
                return Err(Error::ShuttingDown);
            }
            if self.shared.rebuild.active.swap(true, Ordering::SeqCst) {
                return Err(Error::RebuildInProgress(self.shared.id));
            }
            ActiveGuard(Arc::clone(&self.shared.rebuild))
        };
        let copts = CollectionOptions {
            recompress: opts.recompress,
            on_unreadable: opts.on_unreadable,
            workers: opts.workers.unwrap_or(core.limits.rebuild_workers),
            max_inflight_bytes: opts
                .max_inflight_bytes
                .unwrap_or(core.config.rebuild_inflight_bytes),
        };
        tokio::spawn(rebuild_unit(
            Arc::clone(core),
            Arc::clone(&self.shared),
            target,
            copts,
            guard,
        ))
        .await
        .map_err(Error::Join)?
    }
}

/// Test hook: records that `p` was reached, then crashes or pauses there if a test asked.
#[cfg(test)]
async fn point(core: &Core, shared: &Shared, p: RebuildPoint) -> Result<(), Error> {
    let h = &shared.hooks;
    h.rebuild_reached.send_replace(Some(p));
    if *h
        .rebuild_crash
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        == Some(p)
    {
        return Err(Error::Internal("injected rebuild crash"));
    }
    let pause = h
        .rebuild_pause
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if let Some((at, sem)) = pause
        && at == p
        && let Either::Left(Ok(permit)) = race(sem.acquire(), core.closing_signal()).await
    {
        permit.forget();
    }
    Ok(())
}

#[cfg(not(test))]
#[allow(clippy::unused_async)] // Same signature as the test hook.
async fn point(_core: &Core, _shared: &Shared, _p: RebuildPoint) -> Result<(), Error> {
    Ok(())
}

/// Sends `cmd` to the indexer and waits for its reply, racing the shutdown signal.
async fn index_call<T>(
    core: &Core,
    make: impl FnOnce(oneshot::Sender<Result<T, Error>>) -> IndexCmd,
) -> Result<T, Error> {
    let (tx, rx) = oneshot::channel();
    let call = async {
        core.index_link
            .tx
            .send(make(tx))
            .await
            .map_err(|_| Error::IndexUnavailable)?;
        rx.await.map_err(|_| Error::IndexUnavailable)?
    };
    match race(call, core.closing_signal()).await {
        Either::Left(r) => r,
        Either::Right(()) => Err(Error::ShuttingDown),
    }
}

async fn rebuild_unit(
    core: Arc<Core>,
    shared: Arc<Shared>,
    target: Codec,
    copts: CollectionOptions,
    _guard: ActiveGuard,
) -> Result<RebuildReport, Error> {
    let _permit = Arc::clone(&core.rebuild_sem)
        .acquire_owned()
        .await
        .map_err(|_| Error::ShuttingDown)?;
    // Held across every await until the end, on purpose: exclusivity with `rebuild_index`,
    // shutdown and the other admin operations. The writer task never takes it.
    let _admin = shared.admin.lock().await;
    if core.is_closing() {
        return Err(Error::ShuttingDown);
    }
    let lock = core.lock_arc().ok_or(Error::ShuttingDown)?;
    run(&core, &shared, target, copts, &lock).await
}

async fn run(
    core: &Arc<Core>,
    shared: &Arc<Shared>,
    target: Codec,
    copts: CollectionOptions,
    lock: &Arc<masstuffy_durable::DirLock>,
) -> Result<RebuildReport, Error> {
    let id = shared.id;
    let key = CollectionKey::new(id.as_u128());
    // Start.
    let info = {
        let (core, shared, lock) = (Arc::clone(core), Arc::clone(shared), Arc::clone(lock));
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            start_job(&core, &shared, target, copts)
        })
        .await
        .map_err(Error::Join)??
    };

    // Stage (best effort).
    let staged = index_call(core, |reply| IndexCmd::Stage {
        key,
        generation: info.generation,
        from: info.cdx_start,
        reply,
    })
    .await;
    match staged {
        Ok(true) => {}
        Ok(false) | Err(Error::IndexUnavailable | Error::IndexQuery(_)) => warn!(
            collection = %id,
            "the index will re-index the collection after the switch; it is unsearchable meanwhile"
        ),
        Err(e) => return abort(core, shared, lock, key, info.generation, e).await,
    }
    point(core, shared, RebuildPoint::Started).await?;

    // Copy rounds.
    let mut frontier = info.frontier;
    let mut rounds = 0u32;
    loop {
        if core.is_closing() {
            return abort(
                core,
                shared,
                lock,
                key,
                info.generation,
                Error::ShuttingDown,
            )
            .await;
        }
        let end = shared.published.cdx_len.load(Ordering::Acquire);
        if rounds > 0 && (end.saturating_sub(frontier) < CATCHUP_BYTES || rounds >= MAX_ROUNDS) {
            break;
        }
        match copy_round(shared, lock, end).await {
            Ok(f) => frontier = f,
            Err(e) => {
                let err = match e {
                    CopyErr::Gone
                    | CopyErr::Failed(masstuffy_collection::Error::RebuildCancelled) => {
                        if core.is_closing() {
                            Error::ShuttingDown
                        } else {
                            Error::RebuildAborted {
                                collection: id,
                                reason: "the writer reopened the collection",
                            }
                        }
                    }
                    CopyErr::Failed(e) => rebuild_error(id, e),
                };
                return abort(core, shared, lock, key, info.generation, err).await;
            }
        }
        rounds = rounds.saturating_add(1);
    }
    point(core, shared, RebuildPoint::Copied).await?;
    // After the hook: a pause is released by shutdown, which must not reach `Finish`.
    if core.is_closing() {
        return abort(
            core,
            shared,
            lock,
            key,
            info.generation,
            Error::ShuttingDown,
        )
        .await;
    }

    // Finish: the writer task commits the new generation.
    let (tx, rx) = oneshot::channel();
    if shared.tx.send(Msg::Finish(tx)).await.is_err() {
        let e = Error::CollectionUnavailable(id);
        return abort(core, shared, lock, key, info.generation, e).await;
    }
    let FinishDone { report, old } = match rx.await {
        // The writer task stopped without replying. If it died before reading `Finish`, the
        // `Rebuilder` is still in the slot: abort it (the files of generation N+1 are removed).
        // If it had taken it, the slot is empty and nothing is left to do here.
        Err(_) => {
            let e = Error::CollectionUnavailable(id);
            return abort(core, shared, lock, key, info.generation, e).await;
        }
        Ok(Ok(done)) => done,
        Ok(Err(FinishFail::Aborted(reason))) => {
            unstage(core, key, info.generation).await;
            return Err(Error::RebuildAborted {
                collection: id,
                reason,
            });
        }
        Ok(Err(FinishFail::Failed { source, committed })) => {
            if !committed {
                unstage(core, key, info.generation).await;
                if matches!(source, masstuffy_collection::Error::RebuildCancelled)
                    && core.is_closing()
                {
                    return Err(Error::ShuttingDown);
                }
            }
            return Err(Error::Rebuild {
                collection: id,
                committed,
                source: Box::new(source),
            });
        }
    };
    point(core, shared, RebuildPoint::Finished).await?;

    // The index switch: the new generation is staged and activated by the indexer.
    let generation = report.generation;
    let switched = index_call(core, |reply| IndexCmd::AwaitActive {
        key,
        generation,
        reply,
    })
    .await;
    let index_switched = match switched {
        Ok(()) => true,
        Err(Error::ShuttingDown) => false,
        Err(e) => {
            warn!(collection = %id, "the index has not switched yet: {e}");
            false
        }
    };
    point(core, shared, RebuildPoint::Switched).await?;

    // Old-generation locators now give `StaleLocator`; remove the old files.
    shared.published.clear_retired();
    let job_lock = Arc::clone(lock);
    let removed = tokio::task::spawn_blocking(move || {
        let _lock = job_lock;
        old.remove()
    })
    .await
    .map_err(Error::Join)?;
    let old_files_removed = match removed {
        Ok(n) => Some(n),
        Err(e) => {
            warn!(collection = %id, "cannot remove the old generation (the next open will): {e}");
            None
        }
    };
    Ok(RebuildReport {
        copy: report,
        rounds,
        index_switched,
        old_files_removed,
    })
}

/// Blocking: creates the `Rebuilder` with the slot locked (see `RebuildSlot::rb`).
fn start_job(
    core: &Core,
    shared: &Shared,
    target: Codec,
    copts: CollectionOptions,
) -> Result<StartInfo, Error> {
    let slot = &shared.rebuild;
    let mut g = slot.lock();
    if let Some(stale) = g.take() {
        // Left by a panicked unit.
        let _ = stale.abort();
    }
    // Reset only here, under the lock, then look at `closing`: shutdown sends `closing` before it
    // sets `cancel`, so either this sees `closing` or the flag is set again after the reset.
    slot.cancel.store(false, Ordering::SeqCst);
    if core.is_closing() {
        return Err(Error::ShuttingDown);
    }
    let source = shared.published.load();
    let mut rb = Rebuilder::start(
        source,
        target,
        &core.limits.collection_opts,
        copts,
        &*core.resolver,
    )
    .map_err(|e| rebuild_error(shared.id, e))?;
    rb.set_cancel(Arc::clone(&slot.cancel));
    let info = StartInfo {
        generation: rb.report().generation,
        cdx_start: rb.cdx_start(),
        frontier: rb.frontier(),
    };
    *g = Some(rb);
    Ok(info)
}

/// One `copy_until(end)` in the blocking pool; returns the new frontier.
async fn copy_round(
    shared: &Arc<Shared>,
    lock: &Arc<masstuffy_durable::DirLock>,
    end: u64,
) -> Result<u64, CopyErr> {
    let (shared, lock) = (Arc::clone(shared), Arc::clone(lock));
    let joined = tokio::task::spawn_blocking(move || {
        let _lock = lock;
        let mut g = shared.rebuild.lock();
        #[cfg(test)]
        shared.hooks.pause_in_copy();
        let rb = g.as_mut().ok_or(CopyErr::Gone)?;
        rb.copy_until(end)
            .map(|p| p.frontier)
            .map_err(CopyErr::Failed)
    })
    .await;
    match joined {
        Ok(r) => r,
        Err(_) => Err(CopyErr::Failed(masstuffy_collection::Error::Internal(
            "copy task panicked",
        ))),
    }
}

/// Abandons a rebuild that was not committed: deletes the new generation's files, unstages the
/// index, and returns `err`.
async fn abort(
    core: &Arc<Core>,
    shared: &Arc<Shared>,
    lock: &Arc<masstuffy_durable::DirLock>,
    key: CollectionKey,
    generation: u32,
    err: Error,
) -> Result<RebuildReport, Error> {
    let (slot, lock) = (Arc::clone(&shared.rebuild), Arc::clone(lock));
    let id = shared.id;
    let r = tokio::task::spawn_blocking(move || {
        let _lock = lock;
        // An empty slot: a reopen took the `Rebuilder`, and its `Collection::open` deleted the files.
        let rb = slot.lock().take();
        if let Some(rb) = rb
            && let Err(e) = rb.abort()
        {
            warn!(collection = %id, "cannot remove the files of an aborted rebuild: {e}");
        }
    })
    .await;
    if let Err(e) = r {
        warn!(collection = %id, "rebuild abort task failed: {e}");
    }
    // A writer that died after publishing generation N+1 already switched: the indexer may be
    // staging it, so discarding the staged rows would leave the activated generation incomplete.
    if shared.published.load().generation() != generation {
        unstage(core, key, generation).await;
    }
    Err(err)
}

/// Best effort: the index may be gone or lagging.
async fn unstage(core: &Core, key: CollectionKey, generation: u32) {
    if let Err(e) = index_call(core, |reply| IndexCmd::Unstage {
        key,
        generation,
        reply,
    })
    .await
    {
        warn!("cannot unstage generation {generation} of the index: {e}");
    }
}
