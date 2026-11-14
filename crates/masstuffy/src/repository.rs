//! The repository: init/open, registry, create, get, shutdown.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use masstuffy_collection::{Codec, Collection, CollectionId, CreateSpec, RecoveryOptions};
use masstuffy_dictstore::DictStore;
use masstuffy_durable::DirLock;
use masstuffy_index::{CollectionKey, SecondaryIndex};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{info, warn};

use crate::collection::{CollectionHandle, Shared};
use crate::config::{self, Config, Limits, RAW_IMPORT_CHECK_ENV, raw_check_from};
use crate::error::Error;
use crate::index::open_backend;
use crate::indexer::{INDEX_QUEUE, IndexCmd, IndexFeed, IndexLink, IndexerParams, spawn_indexer};
use crate::layout::{self, CONFIG_NAME};
use crate::pool::{CtxPool, PreparerPool};
use crate::read::{Fetch, Fetched, read_located};
use crate::resolver::StoreResolver;
use crate::types::{
    CollectionInfo, LoadedCollection, Located, MAX_REPORTED_UNKNOWN, OpenReport, RawRecord,
    SkipReason, SkippedCollection,
};
use crate::writer::{Msg, Published, WriterParams, WriterTask};

/// State shared by the repository and every collection handle. It never points to a [`Shared`].
pub(crate) struct Core {
    pub(crate) dir: PathBuf,
    pub(crate) config: Config,
    pub(crate) limits: Limits,
    pub(crate) raw_check: masstuffy_collection::RawCheck,
    pub(crate) resolver: Arc<StoreResolver>,
    /// false = open, true = closing. Read-held by put/put_raw/create from admission until their
    /// request is queued (or the create is registered); write-held by shutdown while it flips the
    /// flag.
    pub(crate) gate: Arc<tokio::sync::RwLock<bool>>,
    /// Set at the end of shutdown; get/get_raw/collection() fail with `ShuttingDown` afterwards.
    pub(crate) closed: AtomicBool,
    pub(crate) prepare_sem: Arc<Semaphore>,
    pub(crate) read_sem: Arc<Semaphore>,
    /// One permit is one KiB of input admitted but not yet committed.
    pub(crate) inflight_sem: Arc<Semaphore>,
    pub(crate) read_ctxs: CtxPool,
    /// Set to true by shutdown, right after the gate flips: rebuild and training units stop at
    /// their next check, and every wait of a unit is raced with it.
    pub(crate) closing: tokio::sync::watch::Sender<bool>,
    /// Rebuilds running at once (`Config::max_concurrent_rebuilds`); closed by shutdown.
    pub(crate) rebuild_sem: Arc<Semaphore>,
    /// One training at a time per repository; closed by shutdown.
    pub(crate) train_sem: Arc<Semaphore>,
    /// The secondary index, for the queries and tests. Taken (`None`) by shutdown after the
    /// indexer exited. A handle still held by an in-flight query keeps the index file locked
    /// until that query ends.
    index: RwLock<Option<Arc<dyn SecondaryIndex>>>,
    /// Wakes the indexer and talks to it (barriers, re-index, shutdown).
    pub(crate) index_link: IndexLink,
    #[cfg(test)]
    pub(crate) index_hooks: Arc<crate::indexer::IndexHooks>,
    /// Taken (set to `None`) by shutdown.
    lock: Mutex<Option<Arc<DirLock>>>,
}

impl Core {
    /// True once shutdown has started.
    pub(crate) fn is_closing(&self) -> bool {
        *self.closing.borrow()
    }

    /// Completes when shutdown starts (also if the sender is gone).
    pub(crate) async fn closing_signal(&self) {
        let mut rx = self.closing.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    pub(crate) fn lock_arc(&self) -> Option<Arc<DirLock>> {
        self.lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The secondary index (an `Arc` clone; the lock is released at once).
    ///
    /// # Errors
    ///
    /// `ShuttingDown` once shutdown has taken it.
    pub(crate) fn index(&self) -> Result<Arc<dyn SecondaryIndex>, Error> {
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(Error::ShuttingDown)
    }
}

struct Registry {
    by_slug: HashMap<Arc<str>, CollectionId>,
    by_id: HashMap<CollectionId, Arc<Shared>>,
    /// Slugs of collections skipped at open whose manifest could be read: `create_collection`
    /// refuses them so a later open does not find a duplicate.
    reserved: HashSet<String>,
}

/// Lock order: `Core::gate`, then `Inner::create_lock`, then `Inner::index_rebuild`, then
/// `Core::rebuild_sem` (a permit, rebuild units only), then the `admin` mutexes of collections
/// (several only in ascending [`CollectionId`] order), then `registry`, then the leaves
/// `RebuildSlot::rb`, `Published::snapshot`, the pool mutexes, `Hub::dirty`, `Core::index` and
/// `Inner::index_task` (never held while taking another lock, nor across `.await`;
/// `RebuildSlot::rb` is held during blocking I/O by design and nothing else is taken meanwhile).
/// `Core::train_sem` is taken with no other lock held.
/// Semaphores are acquired after `gate` (inflight before prepare). A std lock is never held
/// across `.await` and a registry guard is dropped before anything else is taken. The indexer
/// task holds no lock across `.await`; its per-collection state is owned by the task.
struct Inner {
    core: Arc<Core>,
    registry: RwLock<Registry>,
    /// Serialises `create_collection` (spec 04 4.1 step 1). Held across the create's
    /// `spawn_blocking` on purpose: slug check, create and registration are one critical section.
    create_lock: Arc<tokio::sync::Mutex<()>>,
    /// Set by the first `shutdown` call, which runs the shutdown in a spawned task so that
    /// cancelling the caller cannot leave it half done.
    shutdown_started: AtomicBool,
    /// Becomes true when the shutdown task has finished.
    shutdown_done: tokio::sync::watch::Sender<bool>,
    /// The shutdown task's error, readable by every caller.
    shutdown_err: Mutex<Option<Arc<Error>>>,
    /// The indexer task; taken by shutdown.
    index_task: Mutex<Option<JoinHandle<()>>>,
    /// Makes `rebuild_index` exclusive with itself. Held across awaits on purpose.
    index_rebuild: tokio::sync::Mutex<()>,
}

impl Inner {
    fn registry(&self) -> std::sync::RwLockReadGuard<'_, Registry> {
        self.registry.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An open repository. Cheap to clone; usable from any task.
///
/// Dropping the last clone and the last [`CollectionHandle`] without calling
/// [`shutdown`](Repository::shutdown) is safe: every writer task commits what was queued, writes
/// its final checkpoint and exits, and the directory lock is released when the last task has
/// exited. If the runtime itself stops first, in-flight blocking commits run to completion and
/// unacknowledged requests are covered by recovery.
#[derive(Clone)]
pub struct Repository {
    inner: Arc<Inner>,
}

fn io_err(op: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io { op, path, source }
}

fn env_raw_check() -> Result<masstuffy_collection::RawCheck, Error> {
    raw_check_from(std::env::var_os(RAW_IMPORT_CHECK_ENV).as_deref())
}

/// Result of the blocking half of the open sequence.
struct Scanned {
    config: Config,
    dicts: Arc<DictStore>,
    dict_report: masstuffy_dictstore::ScanReport,
    candidates: Vec<(CollectionId, PathBuf)>,
    unknown: Vec<PathBuf>,
    unknown_count: u64,
    tmp_removed: usize,
    trash_removed: usize,
}

enum Opened {
    Loaded(
        CollectionId,
        Box<Collection>,
        Box<masstuffy_collection::RecoveryReport>,
    ),
    Skipped(SkippedCollection, Option<String>),
    Trashed(PathBuf),
}

impl Repository {
    /// Creates a repository in `dir` (created if missing; its parent must exist), then opens it,
    /// keeping the lock.
    ///
    /// # Errors
    ///
    /// `Locked`, `NotEmpty`, `AlreadyInitialized`, `InvalidEnv`, `Config`, `Io`, `Durable`,
    /// `Dict`.
    pub async fn init(dir: impl Into<PathBuf>) -> Result<Repository, Error> {
        Repository::init_with_config(dir, &Config::default()).await
    }

    /// Like [`init`](Self::init) with an explicit config (validated first).
    ///
    /// The directory must be empty, or hold only the leftovers of an interrupted `init`.
    /// `config.json` is written last, by atomic replace: it is the commit point.
    ///
    /// # Errors
    ///
    /// As [`init`](Self::init).
    pub async fn init_with_config(
        dir: impl Into<PathBuf>,
        config: &Config,
    ) -> Result<Repository, Error> {
        let dir = dir.into();
        config.validate().map_err(|source| Error::Config {
            path: dir.join(CONFIG_NAME),
            source,
        })?;
        let raw_check = env_raw_check()?;
        let cfg = config.clone();
        let d = dir.clone();
        let lock = spawn(move || init_blocking(&d, &cfg)).await??;
        let (repo, _report) = Repository::open_locked(dir, Arc::new(lock), raw_check).await?;
        Ok(repo)
    }

    /// Opens an existing repository. Invalid collections are skipped and reported; aborted
    /// creates are trashed.
    ///
    /// # Errors
    ///
    /// `Locked`, `NotARepository`, `Layout`, `Config`, `InvalidEnv`, `Io`, `Durable`, `Dict`
    /// (fatal dictionary store scan errors), `Join`.
    pub async fn open(dir: impl Into<PathBuf>) -> Result<(Repository, OpenReport), Error> {
        Repository::open_inner(dir.into(), env_raw_check()?).await
    }

    pub(crate) async fn open_inner(
        dir: PathBuf,
        raw_check: masstuffy_collection::RawCheck,
    ) -> Result<(Repository, OpenReport), Error> {
        let d = dir.clone();
        let lock = spawn(move || DirLock::try_acquire(&d).map_err(Error::from)).await??;
        Repository::open_locked(dir, Arc::new(lock), raw_check).await
    }

    async fn open_locked(
        dir: PathBuf,
        lock: Arc<DirLock>,
        raw_check: masstuffy_collection::RawCheck,
    ) -> Result<(Repository, OpenReport), Error> {
        let d = dir.clone();
        // Every blocking job that mutates repository files owns a clone of the lock, so the
        // directory stays locked even if this future is cancelled while a job runs.
        let job_lock = Arc::clone(&lock);
        let scanned = spawn(move || {
            let _lock = job_lock;
            scan_blocking(&d)
        })
        .await??;
        // Index open (blocking; recreated empty if unreadable).
        let (backend, index_recreated) = {
            let d = dir.clone();
            let cfg = scanned.config.clone();
            let job_lock = Arc::clone(&lock);
            spawn(move || {
                let _lock = job_lock;
                open_backend(&d, &cfg)
            })
            .await??
        };
        let Scanned {
            config,
            dicts,
            dict_report,
            candidates,
            unknown,
            unknown_count,
            tmp_removed,
            trash_removed,
        } = scanned;
        let limits = Limits::new(&config);
        let resolver = Arc::new(StoreResolver(dicts));
        let prepare_permits = limits.prepare_permits;
        let read_permits = limits.read_permits;
        let rebuild_permits = limits.rebuild_permits;
        let inflight = usize::try_from(limits.inflight_kib).unwrap_or(usize::MAX >> 4);
        let open_concurrency = usize::try_from(config.open_concurrency).unwrap_or(1);
        let collection_opts = limits.collection_opts;
        let (tx, rx) = mpsc::channel(INDEX_QUEUE);
        let (index_link, hub) = IndexLink::new(tx);
        let batch_ops = usize::try_from(config.index_batch_ops).unwrap_or(1);
        #[cfg(test)]
        let index_hooks = Arc::new(crate::indexer::IndexHooks::default());
        let core = Arc::new(Core {
            dir: dir.clone(),
            config,
            limits,
            raw_check,
            resolver: Arc::clone(&resolver),
            gate: Arc::new(tokio::sync::RwLock::new(false)),
            closed: AtomicBool::new(false),
            prepare_sem: Arc::new(Semaphore::new(prepare_permits)),
            read_sem: Arc::new(Semaphore::new(read_permits)),
            inflight_sem: Arc::new(Semaphore::new(inflight)),
            read_ctxs: CtxPool::new(read_permits),
            closing: tokio::sync::watch::channel(false).0,
            rebuild_sem: Arc::new(Semaphore::new(rebuild_permits)),
            train_sem: Arc::new(Semaphore::new(1)),
            index: RwLock::new(Some(Arc::clone(&backend.index))),
            index_link,
            #[cfg(test)]
            index_hooks: Arc::clone(&index_hooks),
            lock: Mutex::new(Some(lock)),
        });

        // Open the collections with bounded parallelism.
        let lock_for_jobs = core
            .lock_arc()
            .ok_or(Error::Internal("lock released during open"))?;
        let sem = Arc::new(Semaphore::new(open_concurrency));
        let mut set = JoinSet::new();
        for (id, path) in candidates {
            let permit = Arc::clone(&sem)
                .acquire_owned()
                .await
                .map_err(|_| Error::Internal("open semaphore closed"))?;
            let resolver = Arc::clone(&resolver);
            let job_lock = Arc::clone(&lock_for_jobs);
            set.spawn_blocking(move || {
                let _permit = permit;
                let _lock = job_lock;
                open_one(id, path, &collection_opts, &resolver)
            });
        }
        let mut opened = Vec::new();
        while let Some(r) = set.join_next().await {
            opened.push(r.map_err(Error::Join)?);
        }
        opened.sort_by_key(|o| match o {
            Opened::Loaded(id, ..) => Some(*id),
            _ => None,
        });

        let mut report = OpenReport {
            dicts: dict_report,
            loaded: Vec::new(),
            skipped: Vec::new(),
            trashed: Vec::new(),
            unknown,
            unknown_count,
            tmp_removed,
            trash_removed,
            index_recreated,
        };
        let mut reserved: HashSet<String> = HashSet::new();
        let mut loadable = Vec::new();
        for o in opened {
            match o {
                Opened::Loaded(id, coll, rec) => loadable.push((id, coll, rec)),
                Opened::Skipped(s, slug) => {
                    reserved.extend(slug);
                    report.skipped.push(s);
                }
                Opened::Trashed(p) => report.trashed.push(p),
            }
        }

        // Duplicate slugs: skip every member of a group.
        let mut by_slug: HashMap<String, usize> = HashMap::new();
        for (_, coll, _) in &loadable {
            *by_slug
                .entry(coll.manifest().slug().to_owned())
                .or_insert(0) += 1;
        }
        let mut registry = Registry {
            by_slug: HashMap::new(),
            by_id: HashMap::new(),
            reserved,
        };
        let mut feeds = Vec::new();
        for (id, coll, rec) in loadable {
            let slug = coll.manifest().slug().to_owned();
            if by_slug.get(&slug).copied().unwrap_or(0) > 1 {
                report.skipped.push(SkippedCollection {
                    dir: collection_dir(&dir, id),
                    reason: SkipReason::DuplicateSlug { slug: slug.clone() },
                });
                registry.reserved.insert(slug);
                continue;
            }
            let Some(lock) = core.lock_arc() else {
                return Err(Error::Internal("lock released during open"));
            };
            let shared = spawn_writer(&core, id, *coll, lock);
            registry.by_slug.insert(Arc::clone(&shared.slug), id);
            feeds.push(Arc::clone(&shared.feed));
            info!(
                collection = %id,
                slug = %shared.slug,
                entries = rec.entries,
                scan = ?rec.scan,
                "collection loaded"
            );
            if !rec.is_clean() {
                warn!(collection = %id, report = ?rec, "collection recovered with repairs");
            }
            report.loaded.push(LoadedCollection {
                id,
                slug: Arc::clone(&shared.slug),
                recovery: *rec,
            });
            registry.by_id.insert(id, shared);
        }
        for s in &report.skipped {
            warn!(dir = %s.dir.display(), reason = ?s.reason, "collection skipped");
        }
        for t in &report.trashed {
            warn!(dir = %t.display(), "aborted collection create trashed");
        }
        for u in &report.unknown {
            warn!(path = %u.display(), "unknown entry in data/repository (kept)");
        }

        // Collections skipped at open keep their index rows until the operator fixes them.
        let keep: HashSet<CollectionKey> = report
            .skipped
            .iter()
            .filter_map(|s| {
                s.dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.parse::<CollectionId>().ok())
            })
            .map(|id| CollectionKey::new(id.as_u128()))
            .collect();
        let index_lock = core
            .lock_arc()
            .ok_or(Error::Internal("lock released during open"))?;
        let index_task = spawn_indexer(IndexerParams {
            rx,
            hub,
            backend,
            initial: feeds,
            keep,
            batch_ops,
            lock: index_lock,
            #[cfg(test)]
            hooks: index_hooks,
        });

        let repo = Repository {
            inner: Arc::new(Inner {
                core,
                registry: RwLock::new(registry),
                create_lock: Arc::new(tokio::sync::Mutex::new(())),
                shutdown_started: AtomicBool::new(false),
                shutdown_done: tokio::sync::watch::channel(false).0,
                shutdown_err: Mutex::new(None),
                index_task: Mutex::new(Some(index_task)),
                index_rebuild: tokio::sync::Mutex::new(()),
            }),
        };
        Ok((repo, report))
    }

    fn registry(&self) -> std::sync::RwLockReadGuard<'_, Registry> {
        self.inner.registry()
    }

    pub(crate) fn core(&self) -> &Arc<Core> {
        &self.inner.core
    }

    fn handle(&self, shared: Arc<Shared>) -> CollectionHandle {
        CollectionHandle {
            core: Arc::clone(&self.inner.core),
            shared,
        }
    }

    /// Creates a collection (spec 04 4.1).
    ///
    /// The slug must match `[a-z0-9][a-z0-9_-]{0,127}` and be unused (also by a collection that
    /// was skipped at open). A Zstd codec needs an existing dictionary.
    ///
    /// Once admitted, the create, the writer spawn and the registration run as one spawned
    /// unit: dropping this future cannot leave a created collection unregistered.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `InvalidSlug`, `SlugTaken`, `DictNotFound`, `Collection` (create failed;
    /// the directory is trashed best-effort), `Join`.
    pub async fn create_collection(
        &self,
        slug: &str,
        codec: Codec,
    ) -> Result<CollectionHandle, Error> {
        let core = &self.inner.core;
        // Held until the collection is registered, on purpose: shutdown waits for in-flight
        // creates.
        let gate = Arc::clone(&core.gate).read_owned().await;
        if *gate {
            return Err(Error::ShuttingDown);
        }
        if !layout::is_valid_new_slug(slug) {
            return Err(Error::InvalidSlug {
                slug: slug.to_owned(),
            });
        }
        if let Codec::Zstd { dict_id, .. } = codec
            && !core.resolver.0.contains(dict_id.get())
        {
            return Err(Error::DictNotFound(dict_id.get()));
        }
        let create = Arc::clone(&self.inner.create_lock).lock_owned().await;
        let unit = create_unit(
            Arc::clone(&self.inner),
            slug.to_owned(),
            codec,
            gate,
            create,
        );
        let shared = tokio::spawn(unit).await.map_err(Error::Join)??;
        Ok(self.handle(shared))
    }

    /// Exact slug lookup.
    ///
    /// # Errors
    ///
    /// `UnknownCollection`, `ShuttingDown`.
    pub fn collection(&self, slug: &str) -> Result<CollectionHandle, Error> {
        self.check_open()?;
        let shared = {
            let reg = self.registry();
            reg.by_slug
                .get(slug)
                .and_then(|id| reg.by_id.get(id))
                .cloned()
        };
        shared
            .map(|s| self.handle(s))
            .ok_or_else(|| Error::UnknownCollection(slug.to_owned()))
    }

    /// Lookup by uuid.
    ///
    /// # Errors
    ///
    /// `UnknownCollection`, `ShuttingDown`.
    pub fn collection_by_id(&self, id: CollectionId) -> Result<CollectionHandle, Error> {
        Ok(self.handle(self.shared(id)?))
    }

    pub(crate) fn check_open(&self) -> Result<(), Error> {
        if self.inner.core.closed.load(Ordering::Acquire) {
            Err(Error::ShuttingDown)
        } else {
            Ok(())
        }
    }

    pub(crate) fn shared(&self, id: CollectionId) -> Result<Arc<Shared>, Error> {
        self.check_open()?;
        self.registry()
            .by_id
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::UnknownCollection(id.to_string()))
    }

    /// Every collection, sorted by slug.
    #[must_use]
    pub fn collections(&self) -> Vec<CollectionInfo> {
        let shareds: Vec<Arc<Shared>> = self.registry().by_id.values().cloned().collect();
        let mut v: Vec<CollectionInfo> =
            shareds.into_iter().map(|s| self.handle(s).info()).collect();
        v.sort_by(|a, b| a.slug.cmp(&b.slug));
        v
    }

    /// Reads and parses the record (spec 04 4.3 "Par localisateur") and returns its serialized
    /// WARC bytes: exactly one record, digest verified, `record_id` and `warc_type` cross-checked
    /// with the entry.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `UnknownCollection`, `StaleLocator` (generation differs from the active
    /// one), `NotDecodable` (a `HeaderOnly` raw import whose payload disagrees with its
    /// metadata: use [`get_raw`](Self::get_raw)), `Read`, `Join`.
    pub async fn get(&self, at: &Located) -> Result<Vec<u8>, Error> {
        let shared = self.shared(at.collection)?;
        match read_located(&self.inner.core, &shared, at, Fetch::Decoded).await? {
            Fetched::Decoded(v) => Ok(v),
            Fetched::Raw(_) => Err(Error::Internal("fetch mode")),
        }
    }

    /// The stored bytes and their encoding, without decoding (spec 04 4.3 "En brut").
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `UnknownCollection`, `StaleLocator`, `Read`, `Join`.
    pub async fn get_raw(&self, at: &Located) -> Result<RawRecord, Error> {
        let shared = self.shared(at.collection)?;
        match read_located(&self.inner.core, &shared, at, Fetch::Raw).await? {
            Fetched::Raw(r) => Ok(r),
            Fetched::Decoded(_) => Err(Error::Internal("fetch mode")),
        }
    }

    /// Waits until the secondary index reflects every record committed (acknowledged) before this
    /// call, in every loaded collection, and every collection is active in the index. Does not
    /// wait for durability of the index itself (`index_sync_every > 1`). Cancellation is
    /// harmless: the barrier is dropped.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `IndexUnavailable` (indexer stopped), `IndexLagging` (indexing failed for
    /// these collections; retried at their next commit or the next call).
    pub async fn sync_index(&self) -> Result<(), Error> {
        self.check_open()?;
        index_request(&self.inner.core, IndexCmd::Barrier).await
    }

    /// Rebuilds the secondary index from the CDX of every loaded collection (spec 04 4.7),
    /// concurrently with puts. Each collection is dropped from the index and re-indexed
    /// invisibly, then becomes visible at once. Index collections that are neither loaded nor
    /// skipped at open are dropped. Exclusive with another `rebuild_index` and with admin
    /// operations on any collection. Returns when every collection is re-indexed and active.
    /// Cancellation: the rebuild runs in a spawned unit and continues if this future is dropped.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `IndexUnavailable`, `IndexLagging`, `Join`.
    pub async fn rebuild_index(&self) -> Result<(), Error> {
        self.check_open()?;
        tokio::spawn(rebuild_index_unit(Arc::clone(&self.inner)))
            .await
            .map_err(Error::Join)?
    }

    /// The configuration read at open.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.inner.core.config
    }

    /// The raw-import check in force (from `MASSTUFFY_RAW_IMPORT_CHECK`, read once at open).
    #[must_use]
    pub fn raw_check(&self) -> masstuffy_collection::RawCheck {
        self.inner.core.raw_check
    }

    /// The repository directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.core.dir
    }

    /// Stops admitting puts and creates, lets every admitted put finish, stops every writer task
    /// (each drains its queue and writes a final checkpoint), joins them, then releases the
    /// directory lock. Reads fail with `ShuttingDown` afterwards.
    ///
    /// The shutdown runs in a spawned task: cancelling this future does not interrupt it, and a
    /// later (or concurrent) call waits for it. If the shutdown failed, every call returns
    /// `ShutdownFailed` with the same underlying error.
    ///
    /// # Errors
    ///
    /// `ShutdownFailed` wrapping `Join` (a writer task panicked), reported after every task was
    /// joined.
    pub async fn shutdown(&self) -> Result<(), Error> {
        let inner = &self.inner;
        let mut done = inner.shutdown_done.subscribe();
        if !inner.shutdown_started.swap(true, Ordering::AcqRel) {
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move {
                let body = tokio::spawn(shutdown_body(Arc::clone(&inner)));
                let res = match body.await {
                    Ok(r) => r,
                    Err(e) => Err(Error::Join(e)),
                };
                if let Err(e) = res {
                    *inner
                        .shutdown_err
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(e));
                }
                inner.shutdown_done.send_replace(true);
            });
        }
        done.wait_for(|d| *d)
            .await
            .map_err(|_| Error::Internal("shutdown task vanished"))?;
        let err = inner
            .shutdown_err
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        err.map_or(Ok(()), |e| Err(Error::ShutdownFailed(e)))
    }
}

async fn shutdown_body(inner: Arc<Inner>) -> Result<(), Error> {
    let core = &inner.core;
    let mut gate = core.gate.write().await;
    // Every put and create that held a read guard has queued its request (or registered its
    // collection) by now; any later one sees `true`.
    *gate = true;
    // `closing` is sent before any `cancel` flag is set: a rebuild start job resets `cancel`
    // and then reads `closing`, so it either sees `closing` or the later `cancel = true`.
    core.closing.send_replace(true);
    let mut shareds: Vec<Arc<Shared>> = inner.registry().by_id.values().cloned().collect();
    shareds.sort_by_key(|s| s.id);
    // Before any admin mutex: a running rebuild holds its collection's one until it stops.
    for s in &shareds {
        s.rebuild.cancel.store(true, Ordering::SeqCst);
    }
    let mut admin = Vec::with_capacity(shareds.len());
    let mut acks = Vec::with_capacity(shareds.len());
    for s in &shareds {
        admin.push(s.admin.lock().await);
        let (ack_tx, ack_rx) = oneshot::channel();
        if s.tx.send(Msg::Shutdown(ack_tx)).await.is_ok() {
            acks.push(ack_rx);
        }
    }
    for ack in acks {
        let _ = ack.await; // Err: the task ended without acknowledging.
    }
    let mut failure = None;
    for s in &shareds {
        let handle = s.task.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(h) = handle
            && let Err(e) = h.await
        {
            failure = Some(Error::Join(e));
        }
    }
    // Units still waiting for a permit get `ShuttingDown`.
    core.rebuild_sem.close();
    core.train_sem.close();
    // The writers are gone: nothing can wake the indexer any more. It drains every dirty
    // collection, flushes the index and exits.
    let (ack_tx, ack_rx) = oneshot::channel();
    if core
        .index_link
        .tx
        .send(IndexCmd::Shutdown(ack_tx))
        .await
        .is_ok()
    {
        let _ = ack_rx.await; // Err: the indexer ended without acknowledging.
    }
    let index_task = inner
        .index_task
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(h) = index_task
        && let Err(e) = h.await
    {
        failure.get_or_insert(Error::Join(e));
    }
    core.index
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .take(); // drops the query handle
    core.closed.store(true, Ordering::Release);
    // Every writer task and the indexer have exited. The directory lock is released when the
    // last clone drops: a blocking job still running (a training) keeps it until it ends.
    core.lock
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    drop(admin);
    drop(gate);
    failure.map_or(Ok(()), Err)
}

/// Sends a command carrying a reply channel to the indexer and waits for the reply.
async fn index_request(
    core: &Core,
    make: fn(oneshot::Sender<Result<(), Error>>) -> IndexCmd,
) -> Result<(), Error> {
    let (tx, rx) = oneshot::channel();
    core.index_link
        .tx
        .send(make(tx))
        .await
        .map_err(|_| Error::IndexUnavailable)?;
    rx.await.map_err(|_| Error::IndexUnavailable)?
}

/// `rebuild_index`, run as a spawned unit so that cancelling the caller cannot leave it half done.
async fn rebuild_index_unit(inner: Arc<Inner>) -> Result<(), Error> {
    // Held across the awaits on purpose: exclusivity with another rebuild.
    let _exclusive = inner.index_rebuild.lock().await;
    let mut shareds: Vec<Arc<Shared>> = inner.registry().by_id.values().cloned().collect();
    shareds.sort_by_key(|s| s.id);
    // Admin mutexes in ascending id order (lock order), held until the re-index is done.
    let mut admins = Vec::with_capacity(shareds.len());
    for s in &shareds {
        admins.push(s.admin.lock().await);
    }
    let r = index_request(&inner.core, IndexCmd::Reindex).await;
    drop(admins);
    r
}

/// The create, the writer spawn and the registration of one collection, run in a spawned task
/// so that cancelling `create_collection` cannot separate them. The guards are the gate read
/// guard and the create lock, released when the unit ends.
async fn create_unit(
    inner: Arc<Inner>,
    slug: String,
    codec: Codec,
    _gate: tokio::sync::OwnedRwLockReadGuard<bool>,
    _create: tokio::sync::OwnedMutexGuard<()>,
) -> Result<Arc<Shared>, Error> {
    let core = &inner.core;
    let id = {
        let reg = inner.registry();
        if reg.by_slug.contains_key(slug.as_str()) || reg.reserved.contains(&slug) {
            return Err(Error::SlugTaken { slug });
        }
        let mut found = None;
        for _ in 0..3 {
            let id = CollectionId::from_u128(uuid::Uuid::new_v4().as_u128());
            if !reg.by_id.contains_key(&id) {
                found = Some(id);
                break;
            }
        }
        found.ok_or(Error::Internal("no free collection id"))?
    };
    let path = collection_dir(&core.dir, id);
    let split_threshold = core.config.default_split_threshold;
    let opts = core.limits.collection_opts;
    let resolver = Arc::clone(&core.resolver);
    let lock = core.lock_arc().ok_or(Error::ShuttingDown)?;
    let job_lock = Arc::clone(&lock);
    let coll = spawn(move || {
        let _lock = job_lock;
        Collection::create(
            &path,
            CreateSpec {
                uuid: id,
                slug: &slug,
                codec,
                split_threshold,
            },
            &opts,
            &*resolver,
        )
        .map(|(c, _warcinfo)| c) // The indexer tails the CDX: the wake in `spawn_writer` indexes it.
        .map_err(|source| {
            if path.exists()
                && let Err(e) = masstuffy_durable::trash_dir(&path)
            {
                warn!(dir = %path.display(), "cannot trash failed create: {e}");
            }
            Error::Collection {
                collection: id,
                source,
            }
        })
    })
    .await??;
    let shared = spawn_writer(core, id, coll, lock);
    let mut reg = inner
        .registry
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    reg.by_slug.insert(Arc::clone(&shared.slug), id);
    reg.by_id.insert(id, Arc::clone(&shared));
    Ok(shared)
}

fn collection_dir(dir: &Path, id: CollectionId) -> PathBuf {
    layout::repository_dir(dir).join(id.to_string())
}

/// `spawn_blocking` returning the closure's value; a join failure is `Error::Join`.
async fn spawn<T, F>(f: F) -> Result<T, Error>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(Error::Join)
}

/// Builds the shared state of a collection and spawns its writer task.
fn spawn_writer(
    core: &Arc<Core>,
    id: CollectionId,
    coll: Collection,
    lock: Arc<DirLock>,
) -> Arc<Shared> {
    let slug: Arc<str> = Arc::from(coll.manifest().slug());
    let split = coll.manifest().split_threshold().get();
    let (writer, snapshot) = coll.into_parts();
    let published = Arc::new(Published::new(snapshot, writer.cdx_len()));
    let feed = Arc::new(IndexFeed::new(id, Arc::clone(&published), writer.cdx_len()));
    let queue = usize::try_from(core.config.writer_queue)
        .unwrap_or(1)
        .max(1);
    let (tx, rx) = mpsc::channel(queue);
    let rebuild = Arc::new(crate::rebuild::RebuildSlot::new());
    let dir = collection_dir(&core.dir, id);
    #[cfg(test)]
    let hooks = Arc::new(crate::writer::TestHooks::default());
    let task = WriterTask::new(WriterParams {
        rx,
        writer,
        id,
        dir: dir.clone(),
        opts: core.limits.collection_opts,
        resolver: Arc::clone(&core.resolver),
        published: Arc::clone(&published),
        group_bytes_cap: core.config.max_group_bytes.min(split),
        group_requests_cap: usize::try_from(core.config.max_group_requests)
            .unwrap_or(1)
            .max(1),
        checkpoint_every: core.config.checkpoint_every_bytes,
        lock,
        feed: Arc::clone(&feed),
        link: core.index_link.clone(),
        rebuild: Arc::clone(&rebuild),
        #[cfg(test)]
        hooks: Arc::clone(&hooks),
    });
    let handle = tokio::spawn(task.run());
    // Initial catch-up of a loaded collection, or the warcinfo entry of a new one.
    core.index_link.wake(&feed);
    Arc::new(Shared {
        id,
        slug,
        tx,
        published,
        feed,
        preparers: PreparerPool::new(
            usize::try_from(core.config.preparers_per_collection).unwrap_or(1),
        ),
        admin: tokio::sync::Mutex::new(()),
        rebuild,
        task: Mutex::new(Some(handle)),
        #[cfg(test)]
        hooks,
    })
}

/// `init`, blocking half: returns the repository lock, with every directory durable and
/// `config.json` written.
fn init_blocking(dir: &Path, config: &Config) -> Result<DirLock, Error> {
    match fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(dir).map_err(io_err("mkdir", dir))?;
            let parent = match dir.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            masstuffy_durable::sync_dir(parent)?;
        }
        Err(e) => return Err(io_err("stat", dir)(e)),
    }
    let lock = DirLock::try_acquire(dir)?;
    let config_path = dir.join(CONFIG_NAME);
    match fs::symlink_metadata(&config_path) {
        Ok(_) => {
            return Err(Error::AlreadyInitialized {
                path: dir.to_path_buf(),
            });
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("stat", &config_path)(e)),
    }
    layout::check_init_leftovers(dir)?;
    masstuffy_durable::sweep_tmp(dir)?;
    for p in layout::data_paths(dir) {
        match fs::create_dir(&p) {
            Ok(()) => {}
            // check_init_leftovers proved it is a directory.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err("mkdir", &p)(e)),
        }
    }
    masstuffy_durable::sync_dir(&layout::dict_dir(dir))?;
    masstuffy_durable::sync_dir(&layout::data_dir(dir))?;
    masstuffy_durable::sync_dir(dir)?;
    let json = config.to_json().map_err(|source| Error::Config {
        path: config_path,
        source,
    })?;
    masstuffy_durable::atomic_write(dir, CONFIG_NAME, &json)?; // commit point
    Ok(lock)
}

/// `open`, blocking half: config, layout, sweeps, dictionary store, directory scan.
fn scan_blocking(dir: &Path) -> Result<Scanned, Error> {
    let config = config::load(dir)?;
    layout::check_layout(dir)?;
    let tmp_removed = masstuffy_durable::sweep_tmp(dir)?;
    let repo_dir = layout::repository_dir(dir);
    let trash_removed = masstuffy_durable::sweep_trash(&repo_dir)?;
    let limits = Limits::new(&config);
    let (dicts, dict_report) = DictStore::open(layout::dict_zstd_dir(dir), limits.dict_opts)?;

    let mut candidates = Vec::new();
    let mut unknown = Vec::new();
    let mut unknown_count = 0u64;
    for e in fs::read_dir(&repo_dir).map_err(io_err("read directory", &repo_dir))? {
        let e = e.map_err(io_err("read directory", &repo_dir))?;
        let name = e.file_name();
        if masstuffy_durable::is_reserved(&name) {
            continue;
        }
        let path = e.path();
        let id = name.to_str().and_then(|n| n.parse::<CollectionId>().ok());
        let is_dir = fs::symlink_metadata(&path)
            .map_err(io_err("stat", &path))?
            .is_dir();
        match id {
            Some(id) if is_dir => candidates.push((id, path)),
            _ => {
                unknown_count = unknown_count.saturating_add(1);
                if unknown.len() < MAX_REPORTED_UNKNOWN {
                    unknown.push(path);
                }
            }
        }
    }
    candidates.sort_by_key(|(id, _)| *id);
    Ok(Scanned {
        config,
        dicts: Arc::new(dicts),
        dict_report,
        candidates,
        unknown,
        unknown_count,
        tmp_removed,
        trash_removed,
    })
}

/// Opens one collection directory (blocking).
fn open_one(
    id: CollectionId,
    path: PathBuf,
    opts: &masstuffy_collection::Options,
    resolver: &StoreResolver,
) -> Opened {
    #[cfg(test)]
    crate::testutil::open_hook(&path);
    match Collection::open(&path, opts, &RecoveryOptions::default(), resolver) {
        Ok((coll, report)) => {
            let found = coll.manifest().uuid();
            if found == id {
                Opened::Loaded(id, Box::new(coll), Box::new(report))
            } else {
                Opened::Skipped(
                    SkippedCollection {
                        dir: path,
                        reason: SkipReason::UuidMismatch { manifest: found },
                    },
                    None,
                )
            }
        }
        Err(masstuffy_collection::Error::ManifestMissing { .. }) => {
            match masstuffy_durable::trash_dir(&path) {
                Ok(()) => Opened::Trashed(path),
                Err(e) => Opened::Skipped(
                    SkippedCollection {
                        dir: path,
                        reason: SkipReason::TrashFailed(e),
                    },
                    None,
                ),
            }
        }
        Err(e) => {
            let slug = masstuffy_collection::Manifest::load(&path)
                .ok()
                .map(|m| m.slug().to_owned());
            Opened::Skipped(
                SkippedCollection {
                    dir: path,
                    reason: SkipReason::Open(e),
                },
                slug,
            )
        }
    }
}
