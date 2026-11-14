//! Rebuild of a collection's active generation into the next one.
//!
//! A [`Rebuilder`] copies every non-tombstoned record of the active generation into generation
//! `next_gen_id` with a target codec (None to Zstd, Zstd(D1) to Zstd(D2), Zstd(D1) to Zstd(D1)
//! compaction, Zstd to None, None to None), then commits by replacing `manifest.json` (the single
//! commit point). The old generation keeps serving reads **and writes** during the copy.
//!
//! # Preconditions
//!
//! The caller (the engine) holds the per-collection admin mutex from [`Rebuilder::start`] until
//! `finish`, `abort` or drop returns: no other rebuild, `open`, rename, manifest write or
//! tombstone runs for the collection. `commit`s on the old writer may continue until `finish`.
//! Dropping a `Rebuilder` without `finish` leaves its files; the next `start`, `abort` or
//! `Collection::open` deletes them. If the old writer becomes poisoned, drop (or abort) the
//! `Rebuilder` before reopening the collection.
//!
//! # Engine protocol
//!
//! `start` in `spawn_blocking`; the writer task publishes `Writer::cdx_len()` after each commit;
//! the rebuild task repeats `copy_until(published)` until the remaining delta is small (below
//! 1 MiB of CDX or 8 rounds); then the writer task stops draining its queue and runs
//! `finish(&mut writer)` (final delta, fsyncs, checkpoint, manifest swap), publishes the new
//! writer and snapshot, and calls `OldGeneration::remove`. Items queued for the old generation
//! fail with `WrongGeneration` and are prepared again.
//!
//! # Decisions
//!
//! * Writes continue during a rebuild; tombstones do not. A `T` line seen after the first copied
//!   range fails the rebuild (`TombstoneDuringRebuild`): translating it would need an
//!   old-to-new map of every record.
//! * The `Add` lines of a CDX are in strictly increasing `(segment, offset)` order (data is
//!   appended monotonically and recovery never rewinds a segment), so streaming the CDX in file
//!   order reads the data sequentially with no sort (`LocatorOrder` if violated).
//! * Tombstones of the first range are collected in a sorted vector (two passes over the range).
//! * Every generation starts with its own `warcinfo` record (segment 1, offset 0) holding the
//!   target manifest; the old warcinfo records are copied as ordinary records.
//! * Stored bytes are copied unchanged when the codec and dictionary are the same (unless
//!   `RebuildOptions::recompress`); otherwise a record is decoded and re-encoded, on a bounded pool
//!   of scoped worker threads (`RebuildOptions::workers`). Reads, ordering and appends stay on the
//!   caller's thread and the output is written in CDX order, so it is byte-identical for any
//!   worker count. Threads live inside one `copy_until` call and are joined before it returns.
//! * Damaged records (data errors only) fail the rebuild, or are skipped and reported with
//!   `Unreadable::Skip`. Environment errors always fail.
//! * The new generation is `next_gen_id`, bumped only by the commit. Leftover files of
//!   non-active generations are deleted by `start` as `Collection::open` does.
//! * The old generation's files are deleted by [`OldGeneration::remove`], never by `finish`.
//!
//! # Crash table
//!
//! | stop after | on disk | next `open` or `start` |
//! |---|---|---|
//! | `start` .. `finish` before the manifest swap | generation `N+1` files, manifest says `N` | `N+1` files deleted, `N` intact |
//! | swap not durable (power loss) | manifest may revert to `N` | as above |
//! | swap durable | manifest says `N+1`, both generations on disk | `N` files deleted |
//! | `OldGeneration::remove` part-way | some `N` files left | deleted |
//!
//! Old files are deleted only after `atomic_write` returned, and every new byte is fsynced before it.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;

use masstuffy_cdx::{CdxWriter, Entry, Kind};
use masstuffy_seglog::{Appender, Locator};
use masstuffy_zframe::{CodecError, inspect_frame};

use crate::checkpoint::{self, MAX_CHECKPOINT_SEGMENTS};
use crate::collection::warcinfo_record;
use crate::context::{GenContext, Options};
use crate::entries::collect_tombstones;
use crate::error::{CommitError, Error, PrepareError, ReadError};
use crate::layout::{MANIFEST_NAME, cdx_name, checkpoint_name, create_cdx, segment_naming};
use crate::manifest::{Codec, Generation, Manifest};
use crate::prepare::{Prepared, Preparer};
use crate::range::{LineReader, cdx_header_len};
use crate::recover;
use crate::resolve::DictResolver;
use crate::snapshot::{ReadCtx, Snapshot};
use crate::writer::{ScanState, Writer, entry_for};

/// At most this many skipped records are listed in `RebuildReport::skipped_first`.
pub const MAX_REPORTED_SKIPPED: usize = 32;
/// Largest accepted `RebuildOptions::workers`.
pub const MAX_REBUILD_WORKERS: usize = 256;
/// Default `RebuildOptions::max_inflight_bytes`.
pub const DEFAULT_REBUILD_INFLIGHT_BYTES: u64 = 256 << 20;
/// Records in flight per worker (the reorder window is `workers * JOBS_PER_WORKER`).
const JOBS_PER_WORKER: usize = 4;
/// Largest pending batch of the new generation, in records and stored bytes.
const BATCH_RECORDS: usize = 256;
const BATCH_BYTES: u64 = 8 << 20;
/// Data fsync period, in stored bytes.
const SYNC_BYTES: u64 = 64 << 20;

/// What to do with a record whose stored bytes are damaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unreadable {
    /// Stop the rebuild; nothing is committed. Default.
    #[default]
    Fail,
    /// Do not copy the record and report it in `RebuildReport::skipped`.
    Skip,
}

/// Options of a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildOptions {
    /// Transcode every record even when the stored bytes could be copied unchanged (same codec,
    /// same dictionary), e.g. to apply a new compression level. Default false.
    pub recompress: bool,
    /// Policy for damaged records (data errors only). Default `Fail`.
    pub on_unreadable: Unreadable,
    /// Transcoding threads. 1 runs on the caller's thread with no thread. Default 1; at most
    /// [`MAX_REBUILD_WORKERS`].
    pub workers: NonZeroUsize,
    /// Byte budget of the records in flight in the pool: each weighs its stored size plus twice
    /// its decoded size. A record heavier than the budget runs alone. Default
    /// [`DEFAULT_REBUILD_INFLIGHT_BYTES`]; must be non-zero.
    pub max_inflight_bytes: u64,
}

impl Default for RebuildOptions {
    fn default() -> Self {
        RebuildOptions {
            recompress: false,
            on_unreadable: Unreadable::Fail,
            workers: NonZeroUsize::MIN,
            max_inflight_bytes: DEFAULT_REBUILD_INFLIGHT_BYTES,
        }
    }
}

impl RebuildOptions {
    /// `available_parallelism() / 2`, at least 1 and at most [`MAX_REBUILD_WORKERS`] (1 if it
    /// cannot be queried). The engine passes this; the crate default stays 1.
    #[must_use]
    pub fn suggested_workers() -> NonZeroUsize {
        let n = thread::available_parallelism().map_or(1, |n| n.get() / 2);
        NonZeroUsize::new(n.clamp(1, MAX_REBUILD_WORKERS)).unwrap_or(NonZeroUsize::MIN)
    }
}

/// Why a record was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// The stored bytes do not match the entry's digest.
    Digest,
    /// The record is outside its segment.
    Bounds,
    /// The stored bytes are not one zstd frame.
    Frame,
    /// Decompression failed.
    Decode,
    /// Target codec None, and the record is not one WARC record.
    NotWarc,
}

/// A record not copied under `Unreadable::Skip`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SkippedRecord {
    /// Segment of the record in the old generation.
    pub segment: u32,
    /// Offset of the record in the old generation.
    pub offset: u64,
    /// Its `WARC-Record-ID`.
    pub record_id: String,
    /// Why.
    pub reason: SkipReason,
}

/// Progress of one `copy_until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CopyProgress {
    /// Old-CDX offset copied up to (the next range starts here).
    pub frontier: u64,
    /// Records written to the new generation by this call.
    pub copied: u64,
}

/// What a rebuild did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RebuildReport {
    /// The generation rebuilt.
    pub from_generation: u32,
    /// The new generation.
    pub generation: u32,
    /// Its codec.
    pub codec: Codec,
    /// The new generation's warcinfo (segment 1, offset 0).
    pub warcinfo: Entry<'static>,
    /// Records copied from the old generation (the new warcinfo excluded).
    pub copied: u64,
    /// Of which stored bytes copied unchanged.
    pub raw_copied: u64,
    /// `Add` entries not copied because tombstoned.
    pub dropped_tombstoned: u64,
    /// `T` lines read (dropped: the new CDX has none).
    pub tombstone_lines: u64,
    /// Records skipped under `Unreadable::Skip`.
    pub skipped: u64,
    /// The first `MAX_REPORTED_SKIPPED` of them, in CDX order.
    pub skipped_first: Vec<SkippedRecord>,
    /// Stored bytes read from the old generation (statistics, saturating).
    pub bytes_read: u64,
    /// Stored bytes written to the new generation (statistics, saturating).
    pub bytes_written: u64,
    /// Files of non-active generations deleted by `start` (an interrupted earlier attempt).
    pub removed_leftovers: Vec<PathBuf>,
    /// CDX offset of the new generation's checkpoint written by `finish`, if any.
    pub checkpoint_written: Option<u64>,
}

/// Result of a successful [`Rebuilder::finish`].
#[non_exhaustive]
pub struct Rebuilt {
    /// The writer of the new generation (replaces the old one, which is retired).
    pub writer: Writer,
    /// The snapshot of the new generation (publish it, then drop the old one).
    pub snapshot: Arc<Snapshot>,
    /// What the rebuild did.
    pub report: RebuildReport,
    /// The old generation's files, to delete with `remove` once the new snapshot is published.
    pub old: OldGeneration,
}

/// The files of a generation retired by a committed rebuild.
#[derive(Debug)]
pub struct OldGeneration {
    dir: PathBuf,
    generation: u32,
    codec: Codec,
}

impl OldGeneration {
    /// The retired generation number.
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Deletes `seg.<g>.*`, `index.<g>.cdx` and `scan.<g>.json` (missing files are skipped), then
    /// fsyncs the directory. Returns the number of files removed.
    ///
    /// Idempotent. On error or crash the leftovers are deleted by `Collection::open` or the next
    /// `Rebuilder::start`. Precondition: the old generation's `Writer` was dropped (it is retired
    /// anyway). Readers still holding the old snapshot may get `SegmentMissing` for segments they
    /// had not opened.
    ///
    /// # Errors
    ///
    /// `Segment`, `Io { op: "remove" }`, `Durable`.
    pub fn remove(self) -> Result<usize, Error> {
        remove_generation(&self.dir, self.generation, &self.codec)
    }
}

fn remove_file_if_present(path: &Path) -> Result<bool, Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Io {
            op: "remove",
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Removes the segments, CDX and checkpoint of `generation`.
fn remove_generation(dir: &Path, generation: u32, codec: &Codec) -> Result<usize, Error> {
    let mut n = masstuffy_seglog::remove_all(dir, &segment_naming(generation, codec)?)?;
    let mut any = false;
    for name in [cdx_name(generation), checkpoint_name(generation)] {
        if remove_file_if_present(&dir.join(name))? {
            n = n.saturating_add(1);
            any = true;
        }
    }
    if any {
        masstuffy_durable::sync_dir(dir)?;
    }
    Ok(n)
}

/// An error raised when an outcome is written, once the policy is known.
enum OutcomeErr {
    Read(ReadError),
    Prepare(PrepareError),
}

impl OutcomeErr {
    fn into_error(self, segment: u32, offset: u64) -> Error {
        match self {
            OutcomeErr::Read(source) => Error::RebuildRead {
                segment,
                offset,
                source,
            },
            OutcomeErr::Prepare(source) => Error::RebuildPrepare {
                segment,
                offset,
                source,
            },
        }
    }
}

enum Outcome {
    /// Ready to append (`raw`: stored bytes copied unchanged).
    Ready { p: Prepared, raw: bool },
    /// A data error; the policy is applied when the outcome is written, in order.
    Skippable {
        segment: u32,
        offset: u64,
        record_id: String,
        reason: SkipReason,
        err: OutcomeErr,
    },
    /// An environment error.
    Fatal(Error),
}

/// A record to transcode.
struct Job {
    seq: u64,
    entry: Entry<'static>,
    stored: Vec<u8>,
    weight: u64,
}

struct Done {
    seq: u64,
    weight: u64,
    outcome: Outcome,
}

/// What reading a record gave: work for a worker, or an outcome already known.
enum Stage {
    Work(Job),
    Finished(Outcome),
}

/// Per-thread state: a target `Preparer` (compressor) and a source decompression context.
struct WorkerState {
    preparer: Preparer,
    read_ctx: ReadCtx,
    #[cfg(test)]
    panic_at_seq: Option<u64>,
}

impl WorkerState {
    fn new(ctx: &Arc<GenContext>) -> Result<WorkerState, Error> {
        Ok(WorkerState {
            preparer: Preparer::new(Arc::clone(ctx))?,
            read_ctx: ReadCtx::new(),
            #[cfg(test)]
            panic_at_seq: None,
        })
    }
}

/// Data errors are skippable; environment errors are not.
fn skip_reason(err: &ReadError) -> Option<SkipReason> {
    match err {
        ReadError::DigestMismatch { .. } => Some(SkipReason::Digest),
        ReadError::Segment(masstuffy_seglog::Error::OutOfBounds { .. }) => Some(SkipReason::Bounds),
        ReadError::Frame(_) => Some(SkipReason::Frame),
        ReadError::Codec(c) if !matches!(c, CodecError::ContextAlloc) => Some(SkipReason::Decode),
        _ => None,
    }
}

fn read_failure(e: &Entry<'_>, err: ReadError) -> Outcome {
    match skip_reason(&err) {
        Some(reason) => Outcome::Skippable {
            segment: e.segment,
            offset: e.offset,
            record_id: e.record_id.to_string(),
            reason,
            err: OutcomeErr::Read(err),
        },
        None => Outcome::Fatal(OutcomeErr::Read(err).into_error(e.segment, e.offset)),
    }
}

fn prepare_failure(e: &Entry<'_>, err: PrepareError) -> Outcome {
    if matches!(err, PrepareError::Record(_)) {
        Outcome::Skippable {
            segment: e.segment,
            offset: e.offset,
            record_id: e.record_id.to_string(),
            reason: SkipReason::NotWarc,
            err: OutcomeErr::Prepare(err),
        }
    } else {
        Outcome::Fatal(OutcomeErr::Prepare(err).into_error(e.segment, e.offset))
    }
}

/// Decodes and re-encodes one record (runs on a worker, or inline).
fn work(src: &Snapshot, st: &mut WorkerState, job: Job) -> Done {
    let Job {
        seq,
        entry,
        stored,
        weight,
    } = job;
    #[cfg(test)]
    if st.panic_at_seq == Some(seq) {
        panic!("injected worker panic");
    }
    let outcome = match src.decode_stored(stored, &mut st.read_ctx) {
        Err(err) => read_failure(&entry, err),
        Ok(plain) => match st.preparer.transcode(plain, &entry) {
            Ok(p) => Outcome::Ready { p, raw: false },
            Err(err) => prepare_failure(&entry, err),
        },
    };
    Done {
        seq,
        weight,
        outcome,
    }
}

fn worker_loop(
    src: &Snapshot,
    st: &mut WorkerState,
    rx: &Mutex<Receiver<Job>>,
    tx: &SyncSender<Done>,
    stop: &AtomicBool,
) {
    loop {
        // The lock is held only while waiting for the next job, never while working.
        let job = {
            let Ok(guard) = rx.lock() else {
                return;
            };
            match guard.recv() {
                Ok(j) => j,
                Err(_) => return,
            }
        };
        if stop.load(Ordering::Relaxed) {
            // The caller is shutting down: drain fast.
            continue;
        }
        let (seq, weight) = (job.seq, job.weight);
        match catch_unwind(AssertUnwindSafe(|| work(src, st, job))) {
            Ok(done) => {
                if tx.send(done).is_err() {
                    return;
                }
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                let _ = tx.send(Done {
                    seq,
                    weight,
                    outcome: Outcome::Fatal(Error::WorkerPanicked),
                });
                return;
            }
        }
    }
}

fn bump(counter: &mut u64) -> Result<(), Error> {
    *counter = counter
        .checked_add(1)
        .ok_or(Error::Internal("rebuild counter overflow"))?;
    Ok(())
}

/// Appends `batch` to the new generation (data, then CDX lines; no fsync). Returns the locators
/// and, if `keep_first`, the owned entry of the first item.
fn append_batch(
    appender: &mut Appender,
    cdx: &mut CdxWriter<File>,
    cdx_path: &Path,
    scan: &mut ScanState,
    batch: &[Prepared],
    keep_first: bool,
) -> Result<(Vec<Locator>, Option<Entry<'static>>), Error> {
    let locs = appender.append_batch(batch)?;
    let mut first = None;
    for (i, (p, loc)) in batch.iter().zip(&locs).enumerate() {
        let e = entry_for(p, loc);
        if let Err(err) = cdx.push(&e) {
            cdx.discard_pending();
            return Err(Error::Commit(CommitError::Entry(err)));
        }
        if keep_first && i == 0 {
            first = Some(e.into_owned());
        }
    }
    let written = cdx.pending();
    cdx.write_pending().map_err(|source| Error::CdxWrite {
        path: cdx_path.to_path_buf(),
        source,
    })?;
    if let (Some(f), Some(l)) = (locs.first(), locs.last()) {
        // All items share one segment (I9).
        scan.record(written, batch.len(), f.segment, l.end());
    } else {
        scan.valid = false;
    }
    Ok((locs, first))
}

/// Test-only fault injection of `finish`.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum RebuildFail {
    BeforeManifest,
    AfterManifest,
}

/// A rebuild in progress. See the module docs for the preconditions and the engine protocol.
///
/// `Send`, not `Sync`. Methods run on one thread at a time and are blocking.
pub struct Rebuilder {
    source: Arc<Snapshot>,
    dir: PathBuf,
    old_cdx: PathBuf,
    from_gen: u32,
    /// Manifest committed by `finish`.
    target: Manifest,
    /// `target.to_json()` and a final `\n`: exactly the bytes of `manifest.json`.
    json: Vec<u8>,
    /// Context of the new generation.
    ctx: Arc<GenContext>,
    opts: Options,
    ropts: RebuildOptions,
    raw_copy: bool,
    appender: Appender,
    cdx: CdxWriter<File>,
    cdx_path: PathBuf,
    scan: ScanState,
    /// State of the caller's thread (raw copy, `workers == 1`, the warcinfo).
    inline: WorkerState,
    /// Pool states, created on the first pooled `copy_until` and reused.
    workers: Vec<WorkerState>,
    batch: Vec<Prepared>,
    batch_bytes: u64,
    /// Stored-byte limit of a batch: `BATCH_BYTES`, or the split threshold if smaller (a batch is
    /// contiguous in one segment).
    batch_limit: u64,
    /// Data bytes appended since the last sync.
    unsynced: u64,
    /// Segment of the last appended batch.
    cur_seg: u32,
    /// Old-CDX offset copied up to.
    frontier: u64,
    /// The first range (with its tombstones) was copied.
    first_done: bool,
    /// Last `Add` locator seen (order check).
    last: Option<(u32, u64)>,
    poisoned: bool,
    report: RebuildReport,
    #[cfg(test)]
    pub(crate) fail: Option<RebuildFail>,
    #[cfg(test)]
    pub(crate) peak_inflight_bytes: u64,
    #[cfg(test)]
    pub(crate) peak_inflight: usize,
    #[cfg(test)]
    pub(crate) panic_at_seq: Option<u64>,
}

impl Rebuilder {
    /// Starts a rebuild of `source`'s generation into generation `next_gen_id` with `target`.
    ///
    /// Deletes the files of every non-active generation (an interrupted earlier attempt), creates
    /// the new generation's files and writes its warcinfo record (the target manifest). Nothing
    /// is fsynced yet: `finish` syncs everything before the commit point.
    ///
    /// # Errors
    ///
    /// `InvalidOptions`, `StaleSource("manifest")` (`source` is not the active generation),
    /// `ManifestMissing`, `Manifest`, `Resolve`, `DictMismatch`, `Io`, `Segment`, `Durable`,
    /// `CdxMissing`, `CdxRead`, `CdxGeneration`, `CdxWrite`, `Build`, `Prepare`, `Commit`,
    /// `Internal`.
    pub fn start(
        source: Arc<Snapshot>,
        target: Codec,
        opts: &Options,
        ropts: RebuildOptions,
        resolver: &dyn DictResolver,
    ) -> Result<Rebuilder, Error> {
        if ropts.workers.get() > MAX_REBUILD_WORKERS {
            return Err(Error::InvalidOptions("workers"));
        }
        if ropts.max_inflight_bytes == 0 {
            return Err(Error::InvalidOptions("max_inflight_bytes"));
        }
        let dir = source.reader().dir().to_path_buf();
        let on_disk = Manifest::load(&dir)?;
        if on_disk != **source.manifest() {
            return Err(Error::StaleSource("manifest"));
        }
        let from_gen = on_disk.active().id;
        let old_codec = on_disk.active().codec;
        let generation = on_disk.next_gen_id();
        let next = generation
            .checked_add(1)
            .ok_or(Error::Internal("generation id overflow"))?;
        let manifest_path = dir.join(MANIFEST_NAME);
        let manifest_err = |source| Error::Manifest {
            path: manifest_path.clone(),
            source,
        };
        let target_m = Manifest::new(
            on_disk.uuid(),
            on_disk.slug(),
            on_disk.split_threshold(),
            Generation {
                id: generation,
                codec: target,
            },
            next,
        )
        .map_err(manifest_err)?;
        let mut json = target_m.to_json().map_err(manifest_err)?;
        let ctx = Arc::new(GenContext::new(&target_m, opts, resolver)?);
        let mut inline = WorkerState::new(&ctx)?;
        let raw_copy = !ropts.recompress
            && match (old_codec, target) {
                (Codec::None, Codec::None) => true,
                (Codec::Zstd { dict_id: a, .. }, Codec::Zstd { dict_id: b, .. }) => a == b,
                _ => false,
            };

        // Leftovers of an interrupted attempt. `sweep_tmp` is not called: the old writer may be
        // running an `atomic_write` of its checkpoint right now.
        let scan = recover::scan_dir(&dir, on_disk.active())?;
        let mut removed_leftovers = Vec::new();
        for path in scan.leftovers {
            if remove_file_if_present(&path)? {
                removed_leftovers.push(path);
            }
        }
        if !removed_leftovers.is_empty() {
            masstuffy_durable::sync_dir(&dir)?;
        }

        let old_cdx = source.cdx_path();
        let frontier = cdx_header_len(&old_cdx, from_gen)?;

        let mut appender = Appender::create(
            &dir,
            segment_naming(generation, &target)?,
            on_disk.split_threshold(),
        )?;
        let mut cdx = create_cdx(&dir, generation)?;
        let cdx_path = dir.join(cdx_name(generation));
        let header_len = fs::metadata(&cdx_path)
            .map_err(|source| Error::Io {
                op: "stat",
                path: cdx_path.clone(),
                source,
            })?
            .len();
        let mut scan_state = ScanState {
            cdx_len: header_len,
            entries: 0,
            seg_ends: Vec::new(),
            valid: true,
        };

        let p = inline.preparer.prepare(warcinfo_record(&json)?)?;
        let unsynced = p.stored_len();
        let (_, warcinfo) = append_batch(
            &mut appender,
            &mut cdx,
            &cdx_path,
            &mut scan_state,
            std::slice::from_ref(&p),
            true,
        )?;
        let warcinfo = warcinfo.ok_or(Error::Internal("warcinfo entry"))?;
        json.push(b'\n');

        Ok(Rebuilder {
            source,
            dir,
            old_cdx,
            from_gen,
            target: target_m,
            json,
            ctx,
            opts: *opts,
            ropts,
            raw_copy,
            appender,
            cdx,
            cdx_path,
            scan: scan_state,
            inline,
            workers: Vec::new(),
            batch: Vec::new(),
            batch_bytes: 0,
            batch_limit: BATCH_BYTES.min(on_disk.split_threshold().get()),
            unsynced,
            cur_seg: 1,
            frontier,
            first_done: false,
            last: None,
            poisoned: false,
            report: RebuildReport {
                from_generation: from_gen,
                generation,
                codec: target,
                warcinfo,
                copied: 0,
                raw_copied: 0,
                dropped_tombstoned: 0,
                tombstone_lines: 0,
                skipped: 0,
                skipped_first: Vec::new(),
                bytes_read: 0,
                bytes_written: 0,
                removed_leftovers,
                checkpoint_written: None,
            },
            #[cfg(test)]
            fail: None,
            #[cfg(test)]
            peak_inflight_bytes: 0,
            #[cfg(test)]
            peak_inflight: 0,
            #[cfg(test)]
            panic_at_seq: None,
        })
    }

    /// Old-CDX offset copied up to.
    #[must_use]
    pub fn frontier(&self) -> u64 {
        self.frontier
    }

    /// Progress so far.
    #[must_use]
    pub fn report(&self) -> &RebuildReport {
        &self.report
    }

    /// Copies the old CDX range `[frontier, end)`. `end` is the old writer's `cdx_len()` (any
    /// line boundary of fsynced data at or after the frontier). Runs concurrently with the old
    /// writer. With `workers > 1` the transcoding runs on scoped threads that are joined before
    /// this call returns, on every path.
    ///
    /// The first call with a non-empty range also applies the tombstones it contains. A `T` line
    /// in any later range fails the rebuild.
    ///
    /// Any error poisons the `Rebuilder` (`RebuildPoisoned` from then on): abort it.
    ///
    /// # Errors
    ///
    /// `RebuildPoisoned`, `CdxRange`, `CdxTruncated`, `CdxCorrupt`, `CdxMissing`, `CdxRead`,
    /// `CdxGeneration`, `Io`, `TombstoneDuringRebuild`, `LocatorOrder`, `RebuildRead`,
    /// `RebuildPrepare`, `WorkerPanicked`, `Segment`, `CdxWrite`, `Commit`, `Prepare`, `Internal`.
    pub fn copy_until(&mut self, end: u64) -> Result<CopyProgress, Error> {
        if self.poisoned {
            return Err(Error::RebuildPoisoned);
        }
        let r = self.copy_range(end);
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn copy_range(&mut self, end: u64) -> Result<CopyProgress, Error> {
        if end < self.frontier {
            return Err(Error::CdxRange {
                path: self.old_cdx.clone(),
                from: self.frontier,
                end,
            });
        }
        if end == self.frontier {
            return Ok(CopyProgress {
                frontier: self.frontier,
                copied: 0,
            });
        }
        let ts = if self.first_done {
            Vec::new()
        } else {
            let mut p1 = LineReader::open(&self.old_cdx, self.from_gen, Some(self.frontier), end)?;
            collect_tombstones(&mut p1)?
        };
        let mut lines = LineReader::open(&self.old_cdx, self.from_gen, Some(self.frontier), end)?;
        let before = self.report.copied;
        if self.raw_copy || self.ropts.workers.get() == 1 {
            self.run_inline(&mut lines, &ts)?;
        } else {
            self.run_pool(&mut lines, &ts)?;
        }
        self.flush(false)?;
        self.first_done = true;
        self.frontier = end;
        Ok(CopyProgress {
            frontier: end,
            copied: self.report.copied.saturating_sub(before),
        })
    }

    /// Next `Add` line to copy: handles `T` lines, the order check and tombstoned entries.
    /// Returns the offset of the line, which is then `lines.current()`.
    fn next_live(
        &mut self,
        lines: &mut LineReader,
        ts: &[(u32, u64)],
    ) -> Result<Option<u64>, Error> {
        loop {
            let Some(at) = lines.advance()? else {
                return Ok(None);
            };
            let e = lines.parse_current(at)?;
            match e.kind {
                Kind::Tombstone => {
                    if self.first_done {
                        return Err(Error::TombstoneDuringRebuild { cdx_offset: at });
                    }
                    bump(&mut self.report.tombstone_lines)?;
                }
                Kind::Add => {
                    let loc = (e.segment, e.offset);
                    if self.last.is_some_and(|l| loc <= l) {
                        return Err(Error::LocatorOrder { cdx_offset: at });
                    }
                    self.last = Some(loc);
                    if ts.binary_search(&loc).is_ok() {
                        bump(&mut self.report.dropped_tombstoned)?;
                        continue;
                    }
                    return Ok(Some(at));
                }
            }
        }
    }

    /// Reads the stored bytes of `e` (caller's thread, one positioned read) and either copies
    /// them (raw copy) or prepares a job for a transcoder.
    fn stage(&mut self, e: &Entry<'_>) -> Result<Stage, Error> {
        self.report.bytes_read = self.report.bytes_read.saturating_add(e.raw_size);
        let mut stored = Vec::new();
        if let Err(err) = self.source.read_raw(e, &mut stored) {
            return Ok(Stage::Finished(read_failure(e, err)));
        }
        if self.raw_copy {
            let outcome = match self.inline.preparer.copy_stored(stored, e) {
                Ok(p) => Outcome::Ready { p, raw: true },
                Err(err) => {
                    Outcome::Fatal(OutcomeErr::Prepare(err).into_error(e.segment, e.offset))
                }
            };
            return Ok(Stage::Finished(outcome));
        }
        let cap = self.ctx.max_record_bytes;
        let plain_est = match self.source.context().codec() {
            Codec::None => e.raw_size,
            Codec::Zstd { .. } => inspect_frame(&stored)
                .ok()
                .and_then(|i| i.content_size)
                .map_or(cap, |n| n.min(cap)),
        };
        let weight = e.raw_size.saturating_add(plain_est.saturating_mul(2));
        Ok(Stage::Work(Job {
            seq: 0,
            entry: e.clone().into_owned(),
            stored,
            weight,
        }))
    }

    /// Raw copy and `workers == 1`: everything on the caller's thread.
    fn run_inline(&mut self, lines: &mut LineReader, ts: &[(u32, u64)]) -> Result<(), Error> {
        while let Some(at) = self.next_live(lines, ts)? {
            let stage = {
                let e = lines.parse_current(at)?;
                self.stage(&e)?
            };
            let outcome = match stage {
                Stage::Finished(o) => o,
                Stage::Work(job) => {
                    let src = Arc::clone(&self.source);
                    work(&src, &mut self.inline, job).outcome
                }
            };
            self.deliver(outcome)?;
        }
        Ok(())
    }

    /// `workers > 1`: see the module docs. Scoped threads, joined on every path.
    fn run_pool(&mut self, lines: &mut LineReader, ts: &[(u32, u64)]) -> Result<(), Error> {
        let w = self.ropts.workers.get();
        while self.workers.len() < w {
            let st = WorkerState::new(&self.ctx)?;
            self.workers.push(st);
        }
        #[cfg(test)]
        for st in &mut self.workers {
            st.panic_at_seq = self.panic_at_seq;
        }
        // Detached from `self` so that the workers borrow them while the caller uses `self`.
        let mut workers = std::mem::take(&mut self.workers);
        let source = Arc::clone(&self.source);
        let window = w.saturating_mul(JOBS_PER_WORKER);
        let (job_tx, job_rx) = sync_channel::<Job>(window);
        let job_rx = Mutex::new(job_rx);
        let stop = AtomicBool::new(false);
        let result = thread::scope(|scope| {
            let (done_tx, done_rx) = sync_channel::<Done>(window);
            let mut handles = Vec::with_capacity(w);
            for st in workers.iter_mut().take(w) {
                let done_tx = done_tx.clone();
                let (rx, stop, src) = (&job_rx, &stop, &*source);
                handles.push(scope.spawn(move || worker_loop(src, st, rx, &done_tx, stop)));
            }
            drop(done_tx);
            let r = self.drive(lines, ts, &job_tx, &done_rx, window);
            // Shutdown: workers drain, see the closed channel and exit. Neither side can block on
            // a send (see `drive`), so every join returns.
            stop.store(true, Ordering::Relaxed);
            drop(job_tx);
            drop(done_rx);
            let mut panicked = false;
            for h in handles {
                if h.join().is_err() {
                    panicked = true;
                }
            }
            match r {
                Err(e) if panicked => {
                    if matches!(e, Error::Internal("rebuild pool disconnected")) {
                        Err(Error::WorkerPanicked)
                    } else {
                        Err(e)
                    }
                }
                Ok(()) if panicked => Err(Error::WorkerPanicked),
                r => r,
            }
        });
        self.workers = workers;
        result
    }

    /// The caller's loop of the pool. Every admitted job is counted in `inflight <= window` until
    /// its outcome is delivered, and both channels hold `window` items, so neither the caller's
    /// `send` nor a worker's `send` can block; the caller blocks only in `recv` while
    /// `inflight > 0`, and each admitted job yields exactly one `Done` (a panic is converted).
    fn drive(
        &mut self,
        lines: &mut LineReader,
        ts: &[(u32, u64)],
        job_tx: &SyncSender<Job>,
        done_rx: &Receiver<Done>,
        window: usize,
    ) -> Result<(), Error> {
        let budget = self.ropts.max_inflight_bytes;
        let mut next_seq: u64 = 0;
        let mut next_write: u64 = 0;
        let mut inflight: usize = 0;
        let mut inflight_bytes: u64 = 0;
        let mut reorder: BTreeMap<u64, Done> = BTreeMap::new();
        let mut pending: Option<Job> = None;
        let mut input_done = false;
        loop {
            // a) append every outcome that is next in order
            while let Some(d) = reorder.remove(&next_write) {
                next_write = next_write.saturating_add(1);
                inflight = inflight.saturating_sub(1);
                inflight_bytes = inflight_bytes.saturating_sub(d.weight);
                self.deliver(d.outcome)?;
            }
            if input_done && pending.is_none() && inflight == 0 {
                return Ok(());
            }
            // b) read the next record while the window has room
            if pending.is_none() && !input_done && inflight < window {
                match self.next_live(lines, ts)? {
                    None => input_done = true,
                    Some(at) => {
                        let stage = {
                            let e = lines.parse_current(at)?;
                            self.stage(&e)?
                        };
                        match stage {
                            Stage::Work(mut job) => {
                                job.seq = next_seq;
                                next_seq = next_seq.saturating_add(1);
                                pending = Some(job);
                            }
                            Stage::Finished(outcome) => {
                                // Read errors are ordered with the rest.
                                let seq = next_seq;
                                next_seq = next_seq.saturating_add(1);
                                inflight = inflight.saturating_add(1);
                                reorder.insert(
                                    seq,
                                    Done {
                                        seq,
                                        weight: 0,
                                        outcome,
                                    },
                                );
                            }
                        }
                    }
                }
                continue;
            }
            // c) admit the pending job within the window and the byte budget
            if let Some(j) = &pending
                && inflight < window
                && (inflight == 0 || inflight_bytes.saturating_add(j.weight) <= budget)
            {
                if let Some(j) = pending.take() {
                    inflight = inflight.saturating_add(1);
                    inflight_bytes = inflight_bytes.saturating_add(j.weight);
                    #[cfg(test)]
                    {
                        self.peak_inflight = self.peak_inflight.max(inflight);
                        self.peak_inflight_bytes = self.peak_inflight_bytes.max(inflight_bytes);
                    }
                    job_tx
                        .send(j)
                        .map_err(|_| Error::Internal("rebuild pool closed"))?;
                }
                continue;
            }
            // d) wait for one outcome
            if inflight == 0 {
                return Err(Error::Internal("rebuild pipeline stalled"));
            }
            let d = done_rx
                .recv()
                .map_err(|_| Error::Internal("rebuild pool disconnected"))?;
            // A panic is reported at once: `WorkerPanicked` wins over any lower-sequence error
            // still waiting in `reorder`.
            if matches!(d.outcome, Outcome::Fatal(Error::WorkerPanicked)) {
                return Err(Error::WorkerPanicked);
            }
            reorder.insert(d.seq, d);
        }
    }

    /// Writes one outcome, in CDX order.
    fn deliver(&mut self, outcome: Outcome) -> Result<(), Error> {
        match outcome {
            Outcome::Ready { p, raw } => {
                if raw {
                    bump(&mut self.report.raw_copied)?;
                }
                bump(&mut self.report.copied)?;
                let len = p.stored_len();
                self.report.bytes_written = self.report.bytes_written.saturating_add(len);
                self.batch_bytes = self.batch_bytes.saturating_add(len);
                self.batch.push(p);
                if self.batch.len() >= BATCH_RECORDS || self.batch_bytes >= self.batch_limit {
                    self.flush(false)?;
                }
                Ok(())
            }
            Outcome::Skippable {
                segment,
                offset,
                record_id,
                reason,
                err,
            } => {
                if self.ropts.on_unreadable == Unreadable::Skip {
                    bump(&mut self.report.skipped)?;
                    if self.report.skipped_first.len() < MAX_REPORTED_SKIPPED {
                        self.report.skipped_first.push(SkippedRecord {
                            segment,
                            offset,
                            record_id,
                            reason,
                        });
                    }
                    Ok(())
                } else {
                    Err(err.into_error(segment, offset))
                }
            }
            Outcome::Fatal(e) => Err(e),
        }
    }

    /// Writes the pending batch to the new generation (no fsync unless a limit is reached).
    fn flush(&mut self, keep_first: bool) -> Result<Option<Entry<'static>>, Error> {
        if self.batch.is_empty() {
            return Ok(None);
        }
        let (locs, first) = append_batch(
            &mut self.appender,
            &mut self.cdx,
            &self.cdx_path,
            &mut self.scan,
            &self.batch,
            keep_first,
        )?;
        self.unsynced = self.unsynced.saturating_add(self.batch_bytes);
        let seg = locs.first().map_or(self.cur_seg, |l| l.segment);
        let rolled = seg != self.cur_seg;
        self.cur_seg = seg;
        self.batch.clear();
        self.batch_bytes = 0;
        if rolled || self.unsynced >= SYNC_BYTES {
            self.sync()?;
        }
        Ok(first)
    }

    /// Data fsync, then CDX fsync (data before index).
    fn sync(&mut self) -> Result<(), Error> {
        self.appender.sync()?;
        self.cdx.get_ref().sync_data().map_err(|source| Error::Io {
            op: "fsync",
            path: self.cdx_path.clone(),
            source,
        })?;
        self.unsynced = 0;
        Ok(())
    }

    /// Final catch-up and commit. Requires exclusive use of `writer` (the old generation's) for
    /// its whole duration: the `&mut` borrow is the proof.
    ///
    /// Order: copy the last delta (up to `writer.cdx_len()`), fsync the new data and CDX, write
    /// the new generation's checkpoint, then replace `manifest.json` (the commit point), then
    /// retire `writer`. The old files are not touched: call `Rebuilt::old.remove()` after
    /// publishing the new snapshot and writer.
    ///
    /// On `Err` with `writer.is_poisoned() == false` nothing was committed: the old generation is
    /// still active and writable and the new files are leftovers. On `Err` with the writer
    /// poisoned the outcome is unknown: drop the writer and reopen the collection, which keeps
    /// whichever generation the manifest names.
    ///
    /// # Errors
    ///
    /// `RebuildPoisoned`, `Commit(Poisoned)` (the writer is poisoned), `StaleSource`, every error
    /// of `copy_until`, `Io`, `Segment`, `Durable`, `Internal`.
    pub fn finish(mut self, writer: &mut Writer) -> Result<Rebuilt, Error> {
        if self.poisoned {
            return Err(Error::RebuildPoisoned);
        }
        if writer.is_poisoned() {
            return Err(Error::Commit(CommitError::Poisoned));
        }
        if writer.context().collection() != self.source.context().collection()
            || writer.generation() != self.from_gen
        {
            return Err(Error::StaleSource("writer"));
        }
        if Manifest::load(&self.dir)? != **self.source.manifest() {
            return Err(Error::StaleSource("manifest"));
        }
        self.copy_until(writer.cdx_len())?;
        self.flush(false)?;
        self.sync()?;
        let generation = self.ctx.generation();
        let mut checkpoint_written = None;
        if self.scan.valid
            && self.scan.seg_ends.len() <= MAX_CHECKPOINT_SEGMENTS
            && checkpoint::write(
                &self.dir,
                generation,
                self.scan.cdx_len,
                self.scan.entries,
                &self.scan.seg_ends,
            )?
        {
            checkpoint_written = Some(self.scan.cdx_len);
        }
        #[cfg(test)]
        if matches!(self.fail, Some(RebuildFail::BeforeManifest)) {
            return Err(Error::Internal("injected before manifest"));
        }
        // Commit point.
        if let Err(e) = masstuffy_durable::atomic_write(&self.dir, MANIFEST_NAME, &self.json) {
            // The manifest may or may not have been replaced: only a reopen can tell.
            writer.retire();
            return Err(Error::Durable(e));
        }
        writer.retire();
        #[cfg(test)]
        if matches!(self.fail, Some(RebuildFail::AfterManifest)) {
            return Err(Error::Internal("injected after manifest"));
        }
        let Rebuilder {
            dir,
            target,
            ctx,
            opts,
            appender,
            cdx,
            cdx_path,
            scan,
            mut report,
            source,
            from_gen,
            ..
        } = self;
        report.checkpoint_written = checkpoint_written;
        let mut new_writer = Writer::from_parts(Arc::clone(&ctx), appender, cdx, cdx_path, scan);
        new_writer.last_checkpoint = checkpoint_written.unwrap_or(0);
        let old_codec = source.context().codec();
        let snapshot = Arc::new(Snapshot::new(Arc::new(target), ctx, &dir, &opts)?);
        Ok(Rebuilt {
            writer: new_writer,
            snapshot,
            report,
            old: OldGeneration {
                dir,
                generation: from_gen,
                codec: old_codec,
            },
        })
    }

    /// Deletes the new generation's files (an abandoned rebuild). Returns the number of files
    /// removed.
    ///
    /// # Errors
    ///
    /// `Segment`, `Io { op: "remove" }`, `Durable`.
    pub fn abort(self) -> Result<usize, Error> {
        let dir = self.dir.clone();
        let generation = self.ctx.generation();
        let codec = self.ctx.codec();
        // Close the files first.
        drop(self);
        remove_generation(&dir, generation, &codec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PLAIN_FRAME_MIN;
    use crate::collection::{Collection, CreateSpec};
    use crate::recover::{RecoveryOptions, ScanMode};
    use crate::snapshot::{RawEncoding, ReadCtx};
    use crate::testutil::{
        MapResolver, TestDir, append_cdx_line, base_dict, cdx_lines, clone_dir, commit_batch,
        create_none, create_with, create_zstd, drain, file_len, flip_byte, id_str, identity, live,
        names, none_codec, open_ok, open_with, other_codec, other_dict, read_bytes, record,
        record_of_size, seg1, uuid, with_timeout, zstd_codec,
    };
    use masstuffy_cdx::{ImportMeta, digest};
    use masstuffy_warc::Record;
    use masstuffy_zframe::{Compressor, DecoderDict, Decompressor, EncoderDict};
    use std::borrow::Cow;
    use std::collections::{BTreeMap, HashMap};
    use std::num::NonZeroU64;
    use std::sync::atomic::AtomicU64;

    use crate::DEFAULT_SPLIT_THRESHOLD;
    use crate::error::ReadError;

    fn ro(workers: usize) -> RebuildOptions {
        RebuildOptions {
            workers: NonZeroUsize::new(workers).unwrap(),
            ..RebuildOptions::default()
        }
    }

    fn do_rebuild(
        c: &mut Collection,
        to: Codec,
        ropts: RebuildOptions,
    ) -> Result<(RebuildReport, OldGeneration), Error> {
        c.rebuild(to, &Options::default(), ropts, &MapResolver::standard())
    }

    fn start(c: &Collection, to: Codec, ropts: RebuildOptions) -> Result<Rebuilder, Error> {
        Rebuilder::start(
            Arc::clone(c.snapshot()),
            to,
            &Options::default(),
            ropts,
            &MapResolver::standard(),
        )
    }

    /// Sorted paths of the files of generation `g`.
    fn gen_files(dir: &Path, g: u32) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = names(dir)
            .into_iter()
            .filter(|n| {
                [
                    format!("seg.{g}."),
                    format!("index.{g}."),
                    format!("scan.{g}."),
                ]
                .iter()
                .any(|p| n.starts_with(p.as_str()))
            })
            .map(|n| dir.join(n))
            .collect();
        v.sort();
        v
    }

    struct Fx {
        _d: TestDir,
        dir: PathBuf,
        c: Collection,
        inputs: HashMap<String, Vec<u8>>,
        /// Committed records in order (the warcinfo excluded).
        recs: Vec<Entry<'static>>,
        wi: Entry<'static>,
        wi_bytes: Vec<u8>,
    }

    /// `n` records in 4 batches; record `big_at` is 1.5 MiB.
    fn build(from: Codec, n: usize, big_at: Option<usize>) -> Fx {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, wi) = create_with(&dir, from, DEFAULT_SPLIT_THRESHOLD, &Options::default());
        let wi_bytes = read_bytes(c.snapshot(), &wi);
        let mut p = c.preparer().unwrap();
        let mut inputs = HashMap::new();
        let mut recs = Vec::new();
        let ids: Vec<usize> = (1..=n).collect();
        for chunk in ids.chunks(n.div_ceil(4)) {
            let rs: Vec<Record> = chunk
                .iter()
                .map(|&i| {
                    if Some(i) == big_at {
                        record_of_size(i, 3 << 19)
                    } else {
                        record(i, (i * 379) % 5000)
                    }
                })
                .collect();
            for r in &rs {
                inputs.insert(id_of(r), r.as_bytes().to_vec());
            }
            recs.extend(commit_batch(&mut c, &mut p, rs));
        }
        Fx {
            _d: d,
            dir,
            c,
            inputs,
            recs,
            wi,
            wi_bytes,
        }
    }

    fn id_of(r: &Record) -> String {
        r.view().record_id().unwrap().to_owned()
    }

    fn ids(es: &[Entry<'_>]) -> Vec<String> {
        es.iter().map(|e| e.record_id.to_string()).collect()
    }

    // ---- D. Rebuild.

    #[test]
    fn rebuild_matrix() {
        let t = zstd_codec();
        let cases: [(&str, Codec, Codec, bool); 6] = [
            ("none>zstd", none_codec(), t, false),
            ("zstd>zstd raw", t, t, true),
            ("zstd>other", t, other_codec(), false),
            ("zstd>none", t, none_codec(), false),
            ("none>none raw", none_codec(), none_codec(), true),
            ("zstd>zstd recompress", t, t, false),
        ];
        for workers in [1, 4] {
            for (label, from, to, raw) in cases {
                let recompress = label.ends_with("recompress");
                let mut fx = build(from, 30, Some(17));
                let dead = [fx.recs[3].clone(), fx.recs[10].clone(), fx.recs[24].clone()];
                fx.c.tombstone(&dead).unwrap();
                let ropts = RebuildOptions {
                    recompress,
                    ..ro(workers)
                };
                let (rep, old) = do_rebuild(&mut fx.c, to, ropts).unwrap();
                assert_eq!((rep.generation, rep.from_generation), (2, 1), "{label}");
                assert_eq!(rep.copied, 30 + 1 - 3, "{label}");
                assert_eq!(rep.dropped_tombstoned, 3);
                assert_eq!(rep.tombstone_lines, 3);
                assert_eq!(rep.raw_copied, if raw { rep.copied } else { 0 }, "{label}");
                assert_eq!(rep.skipped, 0);
                assert!(rep.removed_leftovers.is_empty());
                assert!(rep.checkpoint_written.is_some());

                let got = live(&fx.c);
                let live_recs: Vec<&Entry<'static>> = fx
                    .recs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| ![3, 10, 24].contains(i))
                    .map(|(_, e)| e)
                    .collect();
                assert_eq!(got.len(), 2 + live_recs.len());
                assert_eq!((got[0].segment, got[0].offset), (1, 0));
                assert_eq!(got[0], rep.warcinfo);
                assert_eq!(&*got[0].warc_type, "warcinfo");
                assert_eq!(identity(&got[1]), identity(&fx.wi));
                for (g, o) in got[2..].iter().zip(&live_recs) {
                    assert_eq!(identity(g), identity(o), "{label}");
                }
                // The new warcinfo holds the new manifest.
                let on_disk = fs::read(fx.dir.join("manifest.json")).unwrap();
                let body = read_bytes(fx.c.snapshot(), &got[0]);
                let body = String::from_utf8(body).unwrap();
                let json = body
                    .split("\r\n")
                    .find_map(|l| l.strip_prefix("masstuffy-collection-manifest: "))
                    .unwrap();
                assert_eq!([json.as_bytes(), b"\n"].concat(), on_disk);
                assert!(json.contains("\"id\":2") && json.contains("\"next_gen_id\":3"));
                // Byte-exact records, old warcinfo included.
                assert_eq!(read_bytes(fx.c.snapshot(), &got[1]), fx.wi_bytes);
                let mut raw = Vec::new();
                for g in &got[1..] {
                    if g.record_id != fx.wi.record_id {
                        assert_eq!(read_bytes(fx.c.snapshot(), g), fx.inputs[&*g.record_id]);
                    }
                    let enc = fx.c.snapshot().read_raw(g, &mut raw).unwrap();
                    match to {
                        Codec::None => assert_eq!(enc, RawEncoding::Warc),
                        Codec::Zstd { dict_id, .. } => {
                            let big = *g.record_id == id_str(17);
                            let want = if big { 0 } else { dict_id.get() };
                            assert_eq!(enc, RawEncoding::Zstd { dict_id: want }, "{label}");
                        }
                    }
                }
                let m = Manifest::load(&fx.dir).unwrap();
                assert_eq!(
                    (m.active().id, m.active().codec, m.next_gen_id()),
                    (2, to, 3)
                );
                // Old files stay until `remove`.
                assert!(!gen_files(&fx.dir, 1).is_empty());
                assert_eq!(old.generation(), 1);
                assert!(old.remove().unwrap() >= 2);
                let n = names(&fx.dir);
                assert!(
                    n.iter().all(|f| f == "manifest.json" || f.contains(".2.")),
                    "{n:?}"
                );
                // Reopen: clean, checkpoint used, same entries.
                let Fx { _d, dir, c, .. } = fx;
                drop(c);
                let (c, report) = open_ok(&dir);
                assert!(report.is_clean());
                assert!(
                    matches!(report.scan, ScanMode::Partial { .. }),
                    "{:?}",
                    report.scan
                );
                assert_eq!(live(&c), got);
            }
        }
    }

    #[test]
    fn rebuild_twice_same_dict() {
        let mut fx = build(zstd_codec(), 12, None);
        let want = ids(&live(&fx.c));
        let (r1, old1) = do_rebuild(&mut fx.c, zstd_codec(), ro(1)).unwrap();
        assert_eq!(r1.raw_copied, r1.copied);
        old1.remove().unwrap();
        let (r2, old2) = do_rebuild(&mut fx.c, zstd_codec(), ro(1)).unwrap();
        assert_eq!(r2.generation, 3);
        old2.remove().unwrap();
        let got = live(&fx.c);
        // Two new warcinfos on top of the original ones.
        assert_eq!(got.len(), want.len() + 2);
        assert_eq!(ids(&got[2..]), want);
        for e in &got[3..] {
            if &*e.warc_type != "warcinfo" {
                assert_eq!(read_bytes(fx.c.snapshot(), e), fx.inputs[&*e.record_id]);
            }
        }
        assert_eq!(names(&fx.dir).len(), 4);
    }

    /// A committed record with its input bytes.
    type Committed = (Entry<'static>, Vec<u8>);

    fn writes_continue(workers: usize) {
        with_timeout(60, move || {
            let d = TestDir::new();
            let dir = d.sub();
            let opts = Options {
                max_open_files: NonZeroUsize::MIN,
                ..Options::default()
            };
            let thr = NonZeroU64::new(8 * 1024).unwrap();
            let (mut c, _) = create_with(&dir, zstd_codec(), thr, &opts);
            let mut p = c.preparer().unwrap();
            let mut committed: Vec<Committed> = Vec::new();
            for b in 0..10 {
                let rs: Vec<Record> = (0..2).map(|k| record(b * 2 + k + 1, 300)).collect();
                let inputs: Vec<Vec<u8>> = rs.iter().map(|r| r.as_bytes().to_vec()).collect();
                let es = commit_batch(&mut c, &mut p, rs);
                committed.extend(es.into_iter().zip(inputs));
            }
            let (mut writer, snap) = c.into_parts();
            let mut rb = Rebuilder::start(
                Arc::clone(&snap),
                other_codec(),
                &opts,
                ro(workers),
                &MapResolver::standard(),
            )
            .unwrap();
            let published = AtomicU64::new(writer.cdx_len());
            let shared: Mutex<Vec<Committed>> = Mutex::new(committed);
            let done = AtomicBool::new(false);
            let mut writer = thread::scope(|s| {
                let (pubd, sh, dn) = (&published, &shared, &done);
                let wh = s.spawn(move || {
                    for b in 0..60 {
                        let rs: Vec<Record> =
                            (0..2).map(|k| record(1000 + b * 2 + k, 300)).collect();
                        let inputs: Vec<Vec<u8>> =
                            rs.iter().map(|r| r.as_bytes().to_vec()).collect();
                        let batch: Vec<Prepared> =
                            rs.into_iter().map(|r| p.prepare(r).unwrap()).collect();
                        let es = writer.commit(&batch).unwrap();
                        if let Ok(mut g) = sh.lock() {
                            g.extend(es.into_iter().zip(inputs));
                        }
                        pubd.store(writer.cdx_len(), Ordering::Release);
                    }
                    dn.store(true, Ordering::Release);
                    writer
                });
                for seed in 1..=3u64 {
                    let (sn, sh, dn) = (&snap, &shared, &done);
                    s.spawn(move || {
                        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
                        let mut ctx = ReadCtx::new();
                        let mut buf = Vec::new();
                        loop {
                            let fin = dn.load(Ordering::Acquire);
                            let pick = {
                                let g = sh.lock().unwrap();
                                x ^= x << 13;
                                x ^= x >> 7;
                                x ^= x << 17;
                                let i =
                                    usize::try_from(x % u64::try_from(g.len()).unwrap()).unwrap();
                                g[i].clone()
                            };
                            let rec = sn.read(&pick.0, &mut ctx, &mut buf).unwrap();
                            assert_eq!(rec.as_bytes(), &pick.1[..]);
                            if fin {
                                break;
                            }
                        }
                    });
                }
                for _ in 0..5 {
                    rb.copy_until(published.load(Ordering::Acquire)).unwrap();
                }
                wh.join().unwrap()
            });
            let committed = shared.into_inner().unwrap();
            let old_len = writer.cdx_len();
            let mut want: Vec<String> = ids(&drain(&snap, old_len));
            let rebuilt = rb.finish(&mut writer).unwrap();
            want.push(rebuilt.report.warcinfo.record_id.to_string());
            want.sort();
            assert!(writer.is_poisoned());
            let mut old_prep = Preparer::new(Arc::clone(snap.context())).unwrap();
            let stale = vec![old_prep.prepare(record(5000, 10)).unwrap()];
            assert!(matches!(writer.commit(&stale), Err(CommitError::Poisoned)));

            let new_live = drain(&rebuilt.snapshot, rebuilt.writer.cdx_len());
            let mut got = ids(&new_live);
            got.sort();
            assert_eq!(got, want);
            assert_eq!(committed.len(), 20 + 120);
            for (e, input) in &committed {
                let ne = new_live
                    .iter()
                    .find(|n| n.record_id == e.record_id)
                    .unwrap();
                assert_eq!(&read_bytes(&rebuilt.snapshot, ne), input);
            }
            let mut writer2 = rebuilt.writer;
            let mut new_prep = Preparer::new(Arc::clone(rebuilt.snapshot.context())).unwrap();
            let fresh = vec![new_prep.prepare(record(6000, 10)).unwrap()];
            writer2.commit(&fresh).unwrap();

            // Old snapshot after removal: right bytes, or `SegmentMissing` for uncached segments.
            let segs: std::collections::BTreeSet<u32> =
                committed.iter().map(|c| c.0.segment).collect();
            assert!(segs.len() >= 2);
            rebuilt.old.remove().unwrap();
            let (mut missing, mut ctx, mut buf) = (0, ReadCtx::new(), Vec::new());
            for (e, input) in &committed {
                match snap.read(e, &mut ctx, &mut buf) {
                    Ok(r) => assert_eq!(r.as_bytes(), &input[..]),
                    Err(ReadError::SegmentMissing { .. }) => missing += 1,
                    Err(e) => panic!("{e}"),
                }
            }
            assert!(missing >= 1);
        });
    }

    #[test]
    fn rebuild_writes_continue() {
        writes_continue(1);
    }

    #[test]
    fn rebuild_pool_with_catch_up() {
        writes_continue(4);
    }

    #[test]
    fn rebuild_tombstone_during_rebuild_fails() {
        let mut fx = build(zstd_codec(), 8, None);
        let mut rb = start(&fx.c, other_codec(), ro(1)).unwrap();
        rb.copy_until(fx.c.writer().cdx_len()).unwrap();
        fx.c.tombstone(&[fx.recs[2].clone()]).unwrap();
        let r = rb.copy_until(fx.c.writer().cdx_len());
        assert!(matches!(r, Err(Error::TombstoneDuringRebuild { .. })));
        let len = fx.c.writer().cdx_len();
        assert!(matches!(rb.copy_until(len), Err(Error::RebuildPoisoned)));
        assert!(rb.abort().unwrap() >= 2);
        assert!(gen_files(&fx.dir, 2).is_empty());
        let Fx { _d, dir, c, .. } = fx;
        drop(c);
        let (c, rep) = open_ok(&dir);
        assert!(rep.is_clean());
        assert_eq!(live(&c).len(), 1 + 8 - 1);
    }

    #[test]
    fn rebuild_crash_before_commit() {
        // (a) + (c): an abandoned attempt is cleaned by the next `start`.
        let mut fx = build(zstd_codec(), 6, None);
        let mut rb = start(&fx.c, other_codec(), ro(1)).unwrap();
        rb.copy_until(fx.c.writer().cdx_len()).unwrap();
        drop(rb);
        let left = gen_files(&fx.dir, 2);
        assert!(left.len() >= 2);
        let rb = start(&fx.c, other_codec(), ro(1)).unwrap();
        assert_eq!(rb.report().removed_leftovers, left);
        let rebuilt = rb.finish(fx.c.writer()).unwrap();
        assert!(rebuilt.report.copied > 0);
        drop(rebuilt);

        // (a) crash: everything dropped, `open` keeps generation 1.
        let mut fx = build(zstd_codec(), 6, None);
        let mut rb = start(&fx.c, other_codec(), ro(1)).unwrap();
        rb.copy_until(fx.c.writer().cdx_len()).unwrap();
        drop(rb);
        let left = gen_files(&fx.dir, 2);
        let want_a = live(&fx.c);
        let Fx {
            _d,
            dir,
            c,
            recs,
            inputs,
            ..
        } = fx;
        drop(c);
        let (c, rep) = open_ok(&dir);
        assert_eq!((rep.generation, rep.removed.clone()), (1, left));
        assert_eq!(live(&c).len(), 1 + recs.len());
        assert!(gen_files(&dir, 2).is_empty());
        assert_eq!(live(&c), want_a);
        for e in want_a.iter().skip(1) {
            assert_eq!(read_bytes(c.snapshot(), e), inputs[&*e.record_id]);
        }
        drop(_d);

        // (b) failure just before the manifest swap: the old writer is not poisoned.
        let mut fx = build(zstd_codec(), 6, None);
        let mut rb = start(&fx.c, other_codec(), ro(1)).unwrap();
        rb.fail = Some(RebuildFail::BeforeManifest);
        assert!(matches!(
            rb.finish(fx.c.writer()).err(),
            Some(Error::Internal(_))
        ));
        assert!(!fx.c.writer().is_poisoned());
        assert!(
            gen_files(&fx.dir, 2)
                .iter()
                .any(|p| p.ends_with("scan.2.json"))
        );
        let mut p = fx.c.preparer().unwrap();
        let extra = commit_batch(&mut fx.c, &mut p, vec![record(500, 20)]);
        let Fx { _d, dir, c, .. } = fx;
        drop(c);
        let (c, rep) = open_ok(&dir);
        assert_eq!(rep.generation, 1);
        assert!(rep.removed.iter().any(|p| p.ends_with("scan.2.json")));
        assert!(gen_files(&dir, 2).is_empty());
        assert!(live(&c).contains(&extra[0]));
    }

    #[test]
    fn rebuild_crash_after_commit() {
        // (a) finish OK, old files never removed.
        let mut fx = build(zstd_codec(), 6, None);
        let old_files = gen_files(&fx.dir, 1);
        let (rep, old) = do_rebuild(&mut fx.c, other_codec(), ro(1)).unwrap();
        assert_eq!(rep.generation, 2);
        let want = live(&fx.c);
        drop(old);
        let Fx { _d, dir, c, .. } = fx;
        drop(c);
        let (c, rep) = open_ok(&dir);
        assert_eq!(rep.generation, 2);
        assert_eq!(rep.removed, old_files);
        assert_eq!(live(&c), want);
        drop(_d);

        // (b) failure after the swap: the old writer is poisoned, a reopen keeps generation 2.
        let mut fx = build(zstd_codec(), 6, None);
        let old_files = gen_files(&fx.dir, 1);
        let mut rb = start(&fx.c, other_codec(), ro(1)).unwrap();
        rb.fail = Some(RebuildFail::AfterManifest);
        assert!(rb.finish(fx.c.writer()).is_err());
        assert!(fx.c.writer().is_poisoned());
        let Fx { _d, dir, c, .. } = fx;
        drop(c);
        let (c, rep) = open_ok(&dir);
        assert_eq!(rep.generation, 2);
        assert_eq!(rep.removed, old_files);
        assert_eq!(live(&c).len(), 2 + 6);
        for e in live(&c).iter().skip(2) {
            read_bytes(c.snapshot(), e);
        }
    }

    #[test]
    fn rebuild_unreadable() {
        for (label, to) in [("transcode", other_codec()), ("raw", zstd_codec())] {
            for workers in [1, 4] {
                let mut fx = build(zstd_codec(), 12, None);
                let bad = fx.recs[5].clone();
                flip_byte(&seg1(&fx.dir, true, 1), bad.offset + 3);
                let r = do_rebuild(&mut fx.c, to, ro(workers));
                match r {
                    Err(Error::RebuildRead {
                        source: ReadError::DigestMismatch { .. },
                        segment,
                        offset,
                    }) => assert_eq!((segment, offset), (bad.segment, bad.offset), "{label}"),
                    Err(e) => panic!("{label}: {e}"),
                    Ok(_) => panic!("{label}: expected an error"),
                }
                assert_eq!(Manifest::load(&fx.dir).unwrap().active().id, 1);
                let mut p = fx.c.preparer().unwrap();
                commit_batch(&mut fx.c, &mut p, vec![record(700, 10)]);

                let skip = RebuildOptions {
                    on_unreadable: Unreadable::Skip,
                    ..ro(workers)
                };
                let (rep, old) = do_rebuild(&mut fx.c, to, skip).unwrap();
                assert!(!rep.removed_leftovers.is_empty());
                assert_eq!(rep.skipped, 1);
                assert_eq!(rep.skipped_first.len(), 1);
                assert_eq!(rep.skipped_first[0].reason, SkipReason::Digest);
                assert_eq!(rep.skipped_first[0].record_id, bad.record_id);
                assert_eq!(rep.skipped_first[0].offset, bad.offset);
                // old warcinfo + 11 good + the extra commit
                assert_eq!(rep.copied, 1 + 11 + 1);
                old.remove().unwrap();
            }
        }
    }

    fn dict_frame(src: &[u8]) -> Vec<u8> {
        let enc = EncoderDict::new(base_dict(), 1).unwrap();
        let mut out = Vec::new();
        Compressor::new()
            .unwrap()
            .compress(src, &enc, &mut out)
            .unwrap();
        out
    }

    fn meta_for<'a>(rec: &'a Record, stored: &[u8]) -> ImportMeta<'a> {
        let v = rec.view();
        ImportMeta {
            warc_type: Cow::Borrowed(v.warc_type()),
            record_id: Cow::Borrowed(v.record_id().unwrap()),
            url: v.target_uri().map(Cow::Borrowed),
            date: v.date().unwrap(),
            raw_size: u64::try_from(stored.len()).unwrap(),
            digest: Some(digest(stored)),
        }
    }

    /// Imports `payload` with the envelope of `envelope` and `HeaderOnly`.
    fn import_header_only(
        c: &mut Collection,
        p: &mut Preparer,
        envelope: &Record,
        payload: Vec<u8>,
    ) -> Entry<'static> {
        let meta = meta_for(envelope, &payload);
        let prepared = p
            .prepare_raw(&meta, payload, crate::RawCheck::HeaderOnly)
            .unwrap();
        c.writer().commit(&[prepared]).unwrap().remove(0)
    }

    const NOT_WARC: &[u8] = b"this payload is definitely not a WARC record";

    fn decompress_other(raw: &[u8]) -> Vec<u8> {
        let dec = DecoderDict::new(&other_dict()).unwrap();
        let mut out = Vec::new();
        Decompressor::new()
            .unwrap()
            .decompress(raw, Some(&dec), 1 << 30, &mut out)
            .unwrap();
        out
    }

    #[test]
    fn rebuild_header_only_payloads() {
        let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
        for workers in [1, 4] {
            let mut fx = build(zstd_codec(), 3, None);
            let mut p = fx.c.preparer().unwrap();
            let ea = import_header_only(&mut fx.c, &mut p, &record(901, 5), dict_frame(NOT_WARC));
            let good = record(902, 80);
            let eb = import_header_only(
                &mut fx.c,
                &mut p,
                &record(903, 5),
                dict_frame(good.as_bytes()),
            );
            assert!(matches!(
                fx.c.snapshot().read(&ea, &mut ctx, &mut buf),
                Err(ReadError::Record(_))
            ));
            assert!(matches!(
                fx.c.snapshot().read(&eb, &mut ctx, &mut buf),
                Err(ReadError::EntryMismatch("record_id"))
            ));
            let (rep, old) = do_rebuild(&mut fx.c, other_codec(), ro(workers)).unwrap();
            old.remove().unwrap();
            assert_eq!(rep.copied, 1 + 3 + 2);
            let got = live(&fx.c);
            let na = got.iter().find(|e| e.record_id == ea.record_id).unwrap();
            let nb = got.iter().find(|e| e.record_id == eb.record_id).unwrap();
            assert_eq!(identity(na), identity(&ea));
            assert_eq!(identity(nb), identity(&eb));
            let mut raw = Vec::new();
            fx.c.snapshot().read_raw(na, &mut raw).unwrap();
            assert_eq!(decompress_other(&raw), NOT_WARC);
            fx.c.snapshot().read_raw(nb, &mut raw).unwrap();
            assert_eq!(decompress_other(&raw), good.as_bytes());
            assert!(matches!(
                fx.c.snapshot().read(na, &mut ctx, &mut buf),
                Err(ReadError::Record(_))
            ));
            assert!(matches!(
                fx.c.snapshot().read(nb, &mut ctx, &mut buf),
                Err(ReadError::EntryMismatch("record_id"))
            ));

            // Zstd to None: the non-WARC payload cannot be stored.
            let mut fx = build(zstd_codec(), 3, None);
            let mut p = fx.c.preparer().unwrap();
            let ea = import_header_only(&mut fx.c, &mut p, &record(901, 5), dict_frame(NOT_WARC));
            let eb = import_header_only(
                &mut fx.c,
                &mut p,
                &record(903, 5),
                dict_frame(good.as_bytes()),
            );
            match do_rebuild(&mut fx.c, none_codec(), ro(workers)) {
                Err(Error::RebuildPrepare {
                    source: PrepareError::Record(_),
                    segment,
                    offset,
                }) => assert_eq!((segment, offset), (ea.segment, ea.offset)),
                Err(e) => panic!("{e}"),
                Ok(_) => panic!("expected an error"),
            }
            let skip = RebuildOptions {
                on_unreadable: Unreadable::Skip,
                ..ro(workers)
            };
            let (rep, old) = do_rebuild(&mut fx.c, none_codec(), skip).unwrap();
            old.remove().unwrap();
            assert_eq!(rep.skipped, 1);
            assert_eq!(rep.skipped_first[0].reason, SkipReason::NotWarc);
            assert_eq!(rep.skipped_first[0].record_id, ea.record_id);
            let got = live(&fx.c);
            assert!(!got.iter().any(|e| e.record_id == ea.record_id));
            assert!(got.iter().any(|e| e.record_id == eb.record_id));
        }
    }

    #[test]
    fn rebuild_locator_order() {
        let fx = build(none_codec(), 4, None);
        let Fx {
            _d, dir, c, recs, ..
        } = fx;
        drop(c);
        append_cdx_line(&dir.join("index.1.cdx"), &recs[1]);
        let at = cdx_lines(&dir, 1).last().unwrap().0;
        let (mut c, _) = open_ok(&dir);
        let r = do_rebuild(&mut c, none_codec(), ro(1));
        assert!(matches!(r, Err(Error::LocatorOrder { cdx_offset }) if cdx_offset == at));
    }

    #[test]
    fn rebuild_stale_source() {
        let (c, _) = create_zstd(&TestDir::new().sub());
        drop(c);
        let fx = build(zstd_codec(), 3, None);
        let other = Manifest::new(
            uuid(0),
            "renamed",
            DEFAULT_SPLIT_THRESHOLD,
            Generation {
                id: 1,
                codec: zstd_codec(),
            },
            2,
        )
        .unwrap();
        let mut json = other.to_json().unwrap();
        json.push(b'\n');
        masstuffy_durable::atomic_write(&fx.dir, MANIFEST_NAME, &json).unwrap();
        assert!(matches!(
            start(&fx.c, other_codec(), ro(1)).err(),
            Some(Error::StaleSource("manifest"))
        ));

        // `finish` with the writer of another collection.
        let mut fx = build(zstd_codec(), 3, None);
        let d2 = TestDir::new();
        let (mut c2, _) = Collection::create(
            &d2.sub(),
            CreateSpec {
                uuid: uuid(1),
                slug: "other",
                codec: zstd_codec(),
                split_threshold: DEFAULT_SPLIT_THRESHOLD,
            },
            &Options::default(),
            &MapResolver::standard(),
        )
        .unwrap();
        let rb = start(&fx.c, other_codec(), ro(1)).unwrap();
        assert!(matches!(
            rb.finish(c2.writer()).err(),
            Some(Error::StaleSource("writer"))
        ));
        // The right writer still works afterwards.
        let rb = start(&fx.c, other_codec(), ro(1)).unwrap();
        rb.finish(fx.c.writer()).unwrap();
    }

    #[test]
    fn rebuild_rollover() {
        let d = TestDir::new();
        let dir = d.sub();
        let thr = NonZeroU64::new(64 * 1024).unwrap();
        let (mut c, _) = create_with(&dir, none_codec(), thr, &Options::default());
        let mut p = c.preparer().unwrap();
        for b in 0..40 {
            let rs = (0..5).map(|k| record(b * 5 + k + 1, 5000)).collect();
            commit_batch(&mut c, &mut p, rs);
        }
        let want = ids(&live(&c));
        let (rep, old) = do_rebuild(&mut c, none_codec(), ro(1)).unwrap();
        assert_eq!(rep.copied, 201);
        old.remove().unwrap();
        let segs = names(&dir)
            .iter()
            .filter(|n| n.starts_with("seg.2."))
            .count();
        assert!(segs >= 4, "{segs}");
        let got = live(&c);
        for e in &got {
            let len = file_len(&dir.join(format!("seg.2.{}.warc", e.segment)));
            assert!(e.offset + e.raw_size <= len);
        }
        assert_eq!(ids(&got[2..]), want[1..]);
        drop(c);
        let opts = RecoveryOptions {
            full_scan: true,
            ..RecoveryOptions::default()
        };
        let (c, rep) = open_with(&dir, &opts).unwrap();
        assert!(rep.is_clean());
        assert_eq!(live(&c), got);
    }

    #[test]
    fn send_sync_assertions() {
        fn send<T: Send>() {}
        fn send_sync<T: Send + Sync>() {}
        send::<Rebuilder>();
        send::<crate::entries::Entries>();
        send_sync::<crate::PreparedTombstone>();
        send::<OldGeneration>();
        send::<Rebuilt>();
        send_sync::<RebuildReport>();
    }

    // ---- Worker pool.

    /// Mixed-size records for the determinism test.
    fn mixed_sizes(i: usize) -> usize {
        [
            100, 200, 500, 1000, 2000, 4000, 8000, 16_000, 32_000, 64_000,
        ][i % 10]
    }

    #[test]
    fn rebuild_workers_identical() {
        with_timeout(120, || {
            for from in [zstd_codec(), none_codec()] {
                let d = TestDir::new();
                let dir = d.sub();
                let (mut c, _) =
                    create_with(&dir, from, DEFAULT_SPLIT_THRESHOLD, &Options::default());
                let mut p = c.preparer().unwrap();
                let mut es = Vec::new();
                for chunk in (1..=300usize).collect::<Vec<_>>().chunks(25) {
                    let rs = chunk
                        .iter()
                        .map(|&i| match i {
                            100 | 200 => record_of_size(i, 3 << 19),
                            250 => record(i, 300 * 1024),
                            _ => record(i, mixed_sizes(i)),
                        })
                        .collect();
                    es.extend(commit_batch(&mut c, &mut p, rs));
                }
                let tomb: Vec<_> = [7, 50, 120, 201, 290]
                    .iter()
                    .map(|&i| es[i].clone())
                    .collect();
                c.tombstone(&tomb).unwrap();
                drop(c);
                let dir2 = d.path().join("coll2");
                clone_dir(&dir, &dir2);
                let (mut c1, _) = open_ok(&dir);
                let (mut c2, _) = open_ok(&dir2);
                let (r1, o1) = do_rebuild(&mut c1, other_codec(), ro(1)).unwrap();
                let (r4, o4) = do_rebuild(&mut c2, other_codec(), ro(4)).unwrap();
                o1.remove().unwrap();
                o4.remove().unwrap();
                assert_eq!(r1.copied, r4.copied);
                let (l1, l4) = (live(&c1), live(&c2));
                assert_eq!(l1.len(), 2 + 295);
                assert_eq!(l1.len(), l4.len());
                let (mut b1, mut b4) = (Vec::new(), Vec::new());
                for (a, b) in l1.iter().zip(&l4).skip(1) {
                    assert_eq!(identity(a), identity(b));
                    assert_eq!((a.raw_size, a.digest), (b.raw_size, b.digest));
                    c1.snapshot().read_raw(a, &mut b1).unwrap();
                    c2.snapshot().read_raw(b, &mut b4).unwrap();
                    assert_eq!(b1, b4);
                }
                // The warcinfos differ (random id and date, so possibly their compressed size):
                // everything after them is byte-identical.
                let skip1 = usize::try_from(l1[0].raw_size).unwrap();
                let skip4 = usize::try_from(l4[0].raw_size).unwrap();
                let ext = if matches!(other_codec(), Codec::Zstd { .. }) {
                    "warc.zst"
                } else {
                    "warc"
                };
                let s1 = fs::read(dir.join(format!("seg.2.1.{ext}"))).unwrap();
                let s4 = fs::read(dir2.join(format!("seg.2.1.{ext}"))).unwrap();
                assert_eq!(s1[skip1..], s4[skip4..]);
            }
        });
    }

    #[test]
    fn rebuild_pool_preserves_order() {
        with_timeout(60, || {
            let d = TestDir::new();
            let (mut c, _) = create_zstd(&d.sub());
            let mut p = c.preparer().unwrap();
            let rs = (1..=12usize)
                .map(|i| record(i, if i % 2 == 1 { 50 } else { 800 * 1024 }))
                .collect();
            let es = commit_batch(&mut c, &mut p, rs);
            let (_, old) = do_rebuild(&mut c, other_codec(), ro(4)).unwrap();
            old.remove().unwrap();
            let got = live(&c);
            assert_eq!(ids(&got[2..]), ids(&es));
            assert!(
                got.windows(2)
                    .all(|w| { (w[0].segment, w[0].offset) < (w[1].segment, w[1].offset) })
            );
        });
    }

    #[test]
    fn rebuild_pool_error_stops() {
        with_timeout(60, || {
            let mut fx = build(zstd_codec(), 300, None);
            let bad = fx.recs[149].clone();
            flip_byte(&seg1(&fx.dir, true, 1), bad.offset + 3);
            let mut rb = start(&fx.c, other_codec(), ro(4)).unwrap();
            let len = fx.c.writer().cdx_len();
            match rb.copy_until(len) {
                Err(Error::RebuildRead {
                    source: ReadError::DigestMismatch { .. },
                    segment,
                    offset,
                }) => assert_eq!((segment, offset), (bad.segment, bad.offset)),
                Err(e) => panic!("{e}"),
                Ok(_) => panic!("expected an error"),
            }
            assert!(matches!(rb.copy_until(len), Err(Error::RebuildPoisoned)));
            assert_eq!(Manifest::load(&fx.dir).unwrap().active().id, 1);
            rb.abort().unwrap();

            // Decode-type errors: non-WARC payloads rebuilt to None.
            let d = TestDir::new();
            let (mut c, _) = create_zstd(&d.sub());
            let mut p = c.preparer().unwrap();
            commit_batch(&mut c, &mut p, (1..=100).map(|i| record(i, 40)).collect());
            let b1 = import_header_only(&mut c, &mut p, &record(901, 5), dict_frame(NOT_WARC));
            commit_batch(&mut c, &mut p, (101..=150).map(|i| record(i, 40)).collect());
            let b2 = import_header_only(&mut c, &mut p, &record(902, 5), dict_frame(NOT_WARC));
            match do_rebuild(&mut c, none_codec(), ro(4)) {
                Err(Error::RebuildPrepare {
                    source: PrepareError::Record(_),
                    segment,
                    offset,
                }) => assert_eq!((segment, offset), (b1.segment, b1.offset)),
                Err(e) => panic!("{e}"),
                Ok(_) => panic!("expected an error"),
            }
            let skip = RebuildOptions {
                on_unreadable: Unreadable::Skip,
                ..ro(4)
            };
            let (rep, old) = do_rebuild(&mut c, none_codec(), skip).unwrap();
            old.remove().unwrap();
            assert_eq!(rep.skipped, 2);
            let got: Vec<&str> = rep
                .skipped_first
                .iter()
                .map(|s| s.record_id.as_str())
                .collect();
            assert_eq!(got, [&*b1.record_id, &*b2.record_id]);
        });
    }

    #[test]
    fn rebuild_pool_panic_is_error() {
        with_timeout(60, || {
            let mut fx = build(zstd_codec(), 40, None);
            let mut rb = start(&fx.c, other_codec(), ro(4)).unwrap();
            rb.panic_at_seq = Some(3);
            let len = fx.c.writer().cdx_len();
            assert!(matches!(rb.copy_until(len), Err(Error::WorkerPanicked)));
            assert!(matches!(rb.copy_until(len), Err(Error::RebuildPoisoned)));
            rb.abort().unwrap();
            assert!(gen_files(&fx.dir, 2).is_empty());
        });
    }

    #[test]
    fn rebuild_pool_memory_bound() {
        with_timeout(60, || {
            let d = TestDir::new();
            let (mut c, _) = create_zstd(&d.sub());
            let mut p = c.preparer().unwrap();
            for chunk in (1..=200usize).collect::<Vec<_>>().chunks(50) {
                let rs = chunk.iter().map(|&i| record(i, 8000)).collect();
                commit_batch(&mut c, &mut p, rs);
            }
            let big = commit_batch(&mut c, &mut p, vec![record_of_size(300, 3 << 20)]);
            let budget = 100 * 1024;
            let ropts = RebuildOptions {
                max_inflight_bytes: budget,
                ..ro(4)
            };
            let mut rb = start(&c, other_codec(), ropts).unwrap();
            let len = c.writer().cdx_len();
            rb.copy_until(len).unwrap();
            let big_weight = big[0].raw_size + 2 * (3 << 20);
            assert!(
                rb.peak_inflight >= 1 && rb.peak_inflight <= 16,
                "{}",
                rb.peak_inflight
            );
            assert!(rb.peak_inflight_bytes <= budget.max(big_weight));
            let rebuilt = rb.finish(c.writer()).unwrap();
            let got = drain(&rebuilt.snapshot, rebuilt.writer.cdx_len());
            assert_eq!(got.len(), 2 + 201);
            for e in got.iter().skip(2).step_by(17) {
                let want = {
                    let i =
                        usize::from_str_radix(&e.record_id[e.record_id.len() - 12..], 16).unwrap();
                    if i == 300 {
                        record_of_size(300, 3 << 20).as_bytes().to_vec()
                    } else {
                        record(i, 8000).as_bytes().to_vec()
                    }
                };
                assert_eq!(read_bytes(&rebuilt.snapshot, e), want);
            }
        });
    }

    #[test]
    fn rebuild_options_validation() {
        let fx = build(zstd_codec(), 2, None);
        let bad = RebuildOptions {
            workers: NonZeroUsize::new(MAX_REBUILD_WORKERS + 1).unwrap(),
            ..RebuildOptions::default()
        };
        assert!(matches!(
            start(&fx.c, other_codec(), bad).err(),
            Some(Error::InvalidOptions("workers"))
        ));
        let bad = RebuildOptions {
            max_inflight_bytes: 0,
            ..RebuildOptions::default()
        };
        assert!(matches!(
            start(&fx.c, other_codec(), bad).err(),
            Some(Error::InvalidOptions("max_inflight_bytes"))
        ));
        let w = RebuildOptions::suggested_workers().get();
        assert!((1..=MAX_REBUILD_WORKERS).contains(&w));
        let _ = (BTreeMap::<u8, u8>::new(), PLAIN_FRAME_MIN, create_none);
    }
}
