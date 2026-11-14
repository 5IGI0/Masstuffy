//! Dictionary training from a deterministic, memory-bounded sample of a collection's live records
//! (spec 04 4.4, "Générer un dictionnaire").
//!
//! # Sampling
//!
//! Every live entry of the collection's fsynced CDX prefix gets a pseudo-random key,
//! `xxh3(collection uuid ‖ segment ‖ offset)`; the `max_samples` entries with the smallest keys
//! are the sample (bottom-k: independent of the CDX order, `O(k)` memory, deterministic for a
//! given CDX prefix and options). The sample is read in locator order, decoded without parsing
//! it as WARC (`Snapshot::read_raw` + `decode_stored`, skipping dictionary-less frames before decoding), and given to the zstd trainer. The result is installed in
//! the repository dictionary store, which is atomic and idempotent on content (I5: dictionaries
//! are never deleted).
//!
//! # Deviation from the spec ([LEGACY])
//!
//! Spec 4.4 steps 1, 3 and 6 and the table of 5.2 use `data/buffer/` as a cross-process lock and a
//! sample spill. One process per repository is enforced by the directory lock, samples are held in
//! memory under `max_total_bytes`, and concurrency is bounded by a repository-wide semaphore (one
//! training at a time). `data/buffer/` stays in the layout and is unused. Training is read-only for
//! the collection, so it does not take the admin mutex: it delays neither shutdown, nor
//! `rebuild_index`, nor a rebuild (a rebuild that removes the sampled generation makes the
//! training fail with `StaleLocator`; retry).

use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::SystemTime;

use masstuffy_cdx::digest;
use masstuffy_collection::{PLAIN_FRAME_MIN, RawEncoding, ReadCtx, ReadError, Snapshot};
use masstuffy_dictstore::{MAX_LABEL_LEN, is_valid_label};
use masstuffy_warc::WarcDate;
use masstuffy_zframe::{SampleSet, TrainError};

use crate::collection::CollectionHandle;
use crate::error::Error;
use crate::repository::Core;

/// Largest accepted `TrainOptions::max_sample_bytes`.
const MAX_SAMPLE_BYTES_LIMIT: usize = 1 << 20;
/// Largest accepted `TrainOptions::max_samples`.
const MAX_SAMPLES_LIMIT: usize = 1_000_000;
const MIB: u64 = 1 << 20;
/// Closing check period of pass 1, in entries, and of pass 2, in records.
const CHECK_ENTRIES: u64 = 4096;
const CHECK_RECORDS: u64 = 256;

/// Options of [`CollectionHandle::train_dictionary`] (spec 04 4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrainOptions {
    /// Records sampled at most (spec N). Default 100 000. `8..=1_000_000`.
    pub max_samples: usize,
    /// Prefix kept of each sample. Default 128 KiB. `1..=1 MiB`.
    pub max_sample_bytes: usize,
    /// Total sample bytes. Default 128 MiB. `1 MiB..=Config::max_train_bytes`.
    pub max_total_bytes: u64,
    /// Dictionary size cap (spec "taille maximale"). Default 5 MiB.
    /// `256..=Config::dict_max_file_size`.
    pub max_dict_size: usize,
}

impl Default for TrainOptions {
    fn default() -> Self {
        TrainOptions {
            max_samples: 100_000,
            max_sample_bytes: 128 * 1024,
            max_total_bytes: 128 * MIB,
            max_dict_size: 5 * 1024 * 1024,
        }
    }
}

/// What a training run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainReport {
    /// Id of the installed dictionary (use it in `Codec::Zstd`).
    pub dict_id: u32,
    /// False if an identical dictionary was already stored (its file keeps its own label).
    pub created: bool,
    /// Label given to `install` (`<slug>_<YYYYmmddHHMMSS>`, UTC, sanitised).
    pub label: String,
    /// Generation sampled.
    pub generation: u32,
    /// CDX length sampled (the collection state the result depends on).
    pub cdx_len: u64,
    /// Live entries eligible for sampling (warcinfo and stored size > 1 MiB excluded).
    pub candidates: u64,
    /// Samples given to the trainer.
    pub samples: u64,
    /// Total bytes of the samples given to the trainer.
    pub sample_bytes: u64,
    /// Selected records skipped: damaged (digest, frame, codec) or decoded size > 1 MiB.
    pub skipped: u64,
    /// Size of the installed dictionary.
    pub dict_size: u64,
}

impl CollectionHandle {
    /// Trains a zstd dictionary on a deterministic sample of the live records and installs it in
    /// the repository store. Same collection state (generation + CDX prefix) and same options give
    /// the same dictionary bytes and the same id. Read-only for the collection; runs as a spawned
    /// unit (dropping the future does not stop it); one training at a time per repository.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `UnknownCollection` (deleted), `InvalidOptions`, `Read`, `StaleLocator` (a
    /// rebuild removed the sampled generation: retry), `Train`, `Dict`, `Internal`, `Join`.
    pub async fn train_dictionary(&self, opts: TrainOptions) -> Result<TrainReport, Error> {
        let core = &self.core;
        validate(&opts, core)?;
        if core.closed.load(std::sync::atomic::Ordering::Acquire) || core.is_closing() {
            return Err(Error::ShuttingDown);
        }
        self.shared.check_live()?;
        tokio::spawn(train_unit(Arc::clone(core), Arc::clone(&self.shared), opts))
            .await
            .map_err(Error::Join)?
    }
}

fn validate(o: &TrainOptions, core: &Core) -> Result<(), Error> {
    if !(8..=MAX_SAMPLES_LIMIT).contains(&o.max_samples) {
        return Err(Error::InvalidOptions("max_samples"));
    }
    if !(1..=MAX_SAMPLE_BYTES_LIMIT).contains(&o.max_sample_bytes) {
        return Err(Error::InvalidOptions("max_sample_bytes"));
    }
    if !(MIB..=core.config.max_train_bytes).contains(&o.max_total_bytes) {
        return Err(Error::InvalidOptions("max_total_bytes"));
    }
    let dict_cap = usize::try_from(core.config.dict_max_file_size).unwrap_or(usize::MAX);
    if !(256..=dict_cap).contains(&o.max_dict_size) {
        return Err(Error::InvalidOptions("max_dict_size"));
    }
    Ok(())
}

async fn train_unit(
    core: Arc<Core>,
    shared: Arc<crate::collection::Shared>,
    opts: TrainOptions,
) -> Result<TrainReport, Error> {
    // Taken with no other lock held.
    let _permit = Arc::clone(&core.train_sem)
        .acquire_owned()
        .await
        .map_err(|_| Error::ShuttingDown)?;
    if core.is_closing() {
        return Err(Error::ShuttingDown);
    }
    let lock = core.lock_arc().ok_or(Error::ShuttingDown)?;
    let (snap, end) = shared.published.tail();
    let slug = shared.slug();
    tokio::task::spawn_blocking(move || {
        let _lock = lock;
        train_blocking(&core, &shared, &snap, end, &slug, &opts)
    })
    .await
    .map_err(Error::Join)?
}

/// Selected record: `(key, segment, offset)`; the max-heap drops the largest key first, ties by
/// locator.
type Pick = (u64, u32, u64);

fn pick_key(seed: &[u8; 16], segment: u32, offset: u64) -> u64 {
    let mut buf = [0u8; 28];
    buf[..16].copy_from_slice(seed);
    buf[16..20].copy_from_slice(&segment.to_le_bytes());
    buf[20..].copy_from_slice(&offset.to_le_bytes());
    digest(&buf)
}

/// Blocking: samples, trains and installs. See the module docs.
fn train_blocking(
    core: &Core,
    shared: &crate::collection::Shared,
    snap: &Arc<Snapshot>,
    end: u64,
    slug: &str,
    opts: &TrainOptions,
) -> Result<TrainReport, Error> {
    let id = shared.id;
    let read_err = |source: masstuffy_collection::Error| Error::Collection {
        collection: id,
        source,
    };
    let seed: [u8; 16] = id.as_u128().to_le_bytes();

    // Pass 1: the `max_samples` smallest keys among the eligible live entries.
    let mut heap: BinaryHeap<Pick> = BinaryHeap::new();
    let mut candidates = 0u64;
    let mut scanned = 0u64;
    let mut entries = snap.entries(end).map_err(read_err)?;
    while let Some((_, e)) = entries.next_entry().map_err(read_err)? {
        scanned = scanned.saturating_add(1);
        if scanned.is_multiple_of(CHECK_ENTRIES) && core.is_closing() {
            return Err(Error::ShuttingDown);
        }
        if &*e.warc_type == "warcinfo" || e.raw_size > PLAIN_FRAME_MIN {
            continue;
        }
        candidates = candidates.saturating_add(1);
        heap.push((pick_key(&seed, e.segment, e.offset), e.segment, e.offset));
        if heap.len() > opts.max_samples {
            heap.pop();
        }
    }
    drop(entries);
    let mut chosen: Vec<(u32, u64)> = heap.into_iter().map(|(_, s, o)| (s, o)).collect();
    chosen.sort_unstable();

    // Pass 2: read in locator order.
    let mut set = SampleSet::new(masstuffy_zframe::TrainOptions {
        max_samples: opts.max_samples,
        max_total_bytes: opts.max_total_bytes,
        max_sample_bytes: opts.max_sample_bytes,
        max_dict_size: opts.max_dict_size,
    });
    let mut ctx = ReadCtx::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut raw: Vec<u8> = Vec::new();
    let mut skipped = 0u64;
    let mut next = 0usize;
    let mut seen = 0u64;
    let mut entries = snap.entries(end).map_err(read_err)?;
    'walk: while let Some((_, e)) = entries.next_entry().map_err(read_err)? {
        let at = (e.segment, e.offset);
        // Both sequences are in locator order: advance past picks that were not met (a CDX out of
        // locator order loses them).
        while chosen.get(next).is_some_and(|&c| c < at) {
            next += 1;
        }
        match chosen.get(next) {
            None => break,
            Some(&c) if c != at => continue,
            Some(_) => {}
        }
        next += 1;
        seen = seen.saturating_add(1);
        if seen.is_multiple_of(CHECK_RECORDS) && core.is_closing() {
            return Err(Error::ShuttingDown);
        }
        buf.clear();
        match fetch_sample(snap, &e, &mut ctx, &mut buf, &mut raw) {
            // A dictionary-less frame holds a record over 1 MiB: not a sample.
            Ok(true) => skipped = skipped.saturating_add(1),
            Ok(false) => {
                let big = u64::try_from(buf.len()).map_or(true, |n| n > PLAIN_FRAME_MIN);
                if big {
                    skipped = skipped.saturating_add(1);
                } else if matches!(set.push(&buf), Err(TrainError::Full)) {
                    break 'walk;
                }
            }
            Err(ReadError::DigestMismatch { .. } | ReadError::Frame(_) | ReadError::Codec(_)) => {
                skipped = skipped.saturating_add(1)
            }
            Err(source @ ReadError::SegmentMissing { .. }) => {
                let cur = shared.published.load();
                if cur.generation() != snap.generation() {
                    return Err(Error::StaleLocator {
                        expected: cur.generation(),
                        got: snap.generation(),
                    });
                }
                return Err(Error::Read {
                    collection: id,
                    source,
                });
            }
            Err(source) => {
                return Err(Error::Read {
                    collection: id,
                    source,
                });
            }
        }
    }
    drop(entries);
    drop(buf);

    if core.is_closing() {
        return Err(Error::ShuttingDown);
    }
    let samples = u64::try_from(set.len()).unwrap_or(u64::MAX);
    let sample_bytes = set.total_bytes();
    let dict = masstuffy_zframe::train(&set).map_err(Error::Train)?; // CPU heavy
    drop(set);

    let now = WarcDate::from_system_time(SystemTime::now()).ok_or(Error::Internal("clock"))?;
    let label = make_label(slug, &now);
    if !is_valid_label(&label) {
        return Err(Error::Internal("label"));
    }
    if core.is_closing() {
        return Err(Error::ShuttingDown);
    }
    let installed = core.resolver.0.install(&label, &dict)?;
    Ok(TrainReport {
        dict_id: installed.dict_id,
        created: installed.created,
        label,
        generation: snap.generation(),
        cdx_len: end,
        candidates,
        samples,
        sample_bytes,
        skipped,
        dict_size: u64::try_from(dict.len()).unwrap_or(u64::MAX),
    })
}

/// Reads and decodes the record of `e` into `buf`. In a Zstd collection a frame without
/// dictionary (`dict_id == 0`, a record over 1 MiB) is recognised from its header and skipped
/// before decoding: returns `Ok(true)` then.
fn fetch_sample(
    snap: &Snapshot,
    e: &masstuffy_cdx::Entry<'_>,
    ctx: &mut ReadCtx,
    buf: &mut Vec<u8>,
    raw: &mut Vec<u8>,
) -> Result<bool, ReadError> {
    match snap.read_raw(e, raw)? {
        RawEncoding::Warc => {
            std::mem::swap(buf, raw);
            Ok(false)
        }
        RawEncoding::Zstd { dict_id: 0 } => Ok(true),
        RawEncoding::Zstd { .. } => {
            *buf = snap.decode_stored(std::mem::take(raw), ctx)?;
            Ok(false)
        }
    }
}

/// `<slug>_<YYYYmmddHHMMSS>`: every byte of `slug` outside `[A-Za-z0-9_-]` becomes `_`, the slug
/// is truncated so that the label fits `MAX_LABEL_LEN`.
pub(crate) fn make_label(slug: &str, now: &WarcDate) -> String {
    const STAMP: usize = 15; // `_` and 14 digits
    let keep = MAX_LABEL_LEN - STAMP;
    let mut label = String::with_capacity(MAX_LABEL_LEN);
    for b in slug.bytes().take(keep) {
        label.push(if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            char::from(b)
        } else {
            '_'
        });
    }
    // Truncating `bytes()` of a multi-byte slug may cut a char: every byte outside the set maps
    // to `_` one by one, so the label is ASCII and byte truncation is safe.
    label.push('_');
    label.push_str(&format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        now.year(),
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    ));
    label
}
