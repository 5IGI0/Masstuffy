//! Crash recovery of a collection directory: `Collection::open`.
//!
//! # Precondition
//!
//! `open` takes **no lock**. The caller guarantees exclusive access to the directory: no other
//! process uses the repository (the engine holds the repository `DirLock`), and in this process no
//! other `Collection`, `Writer`, rebuild or manifest rewrite exists or runs for the collection
//! (the engine reopens under the per-collection admin mutex); the previous `Writer`, if any, has
//! been dropped. This is what makes deleting every non-active generation safe.
//!
//! # Steps
//!
//! Phase A only reads (plus one directory fsync); if it fails nothing on disk changed:
//!
//! 1. the manifest, the generation context (dictionaries), then an fsync of the directory so the
//!    manifest we read is durable before any decision is based on it;
//! 2. a read-only classification of the directory (leftover generations, unknown files);
//! 3. `Appender::open` (a missing segment or a gap is corruption, never repaired) and the length of
//!    every active segment;
//! 4. the checkpoint, if any, and the CDX scan: the header is always read and checked (I4); the
//!    lines are scanned from the checkpoint when it is valid, else from the header. Every scanned
//!    line gets the cheap I2 check (segment exists, `offset + raw_size` within its length);
//! 5. the deep I2 check (one pread + digest) of the last `verify_last` scanned `Add` entries.
//!
//! Phase B mutates, each step crash-safe and idempotent:
//!
//! 6. the CDX is truncated (and fsynced) at the torn tail, or at the first corrupt or I2-violating
//!    line with [`RepairPolicy::TruncateAtCorruption`];
//! 7. the checkpoint is removed if a repair cut made it stale, or (re)written after a full scan or
//!    at least [`CHECKPOINT_MIN_GROWTH`] bytes of newly scanned CDX;
//! 8. `.tmp-*` leftovers are swept; files of every generation other than the active one
//!    (`seg.<g>.*`, `index.<g>.cdx`, `scan.<g>.json`) are deleted, then the directory is fsynced.
//!
//! # Tests
//!
//! The crash-injection loops use every byte only where an open is cheap (a checkpoint is present,
//! so nothing is rewritten). Without a checkpoint each open writes one (several fsyncs), so those
//! loops use a stride plus every line and record boundary, each with the bytes just before and
//! after it (`b-1, b, b+1`), which are the offsets where behaviour changes.
//!
//! # Policy
//!
//! * A torn tail is always truncated, whatever the policy: it is the normal result of a crash
//!   during the CDX write of an unacknowledged batch. A prefix of that batch may survive
//!   (at-least-once); each of its lines points to data fsynced before the CDX write.
//! * A checksum-valid line pointing past the data or to a missing segment cannot come from a
//!   crash, so it is corruption: an error unless repaired. Repair drops that line and every later
//!   one, possibly acknowledged entries. Segments are never modified.
//! * A partial scan does not re-read the checkpointed prefix, so bit rot in it is not seen by
//!   `open`: it is caught on read (every read checks the digest) or by `full_scan: true`. The
//!   deep check covers only the scanned range; entries before the window are only bounds-checked.
//! * Unknown files are kept and reported, never an error.
//! * A torn, missing or invalid checkpoint is a full scan, never an error. Invariant: while a
//!   `Writer` exists, `scan.<gen>.json` (if any) has `cdx_offset` at most the fsynced CDX length
//!   and describes that prefix.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use masstuffy_cdx::{CdxReader, CdxWriter, DEFAULT_MAX_LINE, Entry, Kind, LineError, Step, Tail};
use masstuffy_seglog::{Appender, Locator};

use crate::checkpoint::{self, CHECKPOINT_MIN_GROWTH, CheckpointV1, MAX_CHECKPOINT_SEGMENTS};
use crate::context::{GenContext, Options};
use crate::error::{Error, I2Violation};
use crate::layout::{FileClass, cdx_name, checkpoint_name, classify, segment_naming};
use crate::manifest::{Codec, Generation, Manifest};
use crate::resolve::DictResolver;
use crate::snapshot::Snapshot;
use crate::writer::{ScanState, Writer};

/// Default for [`RecoveryOptions::verify_last`].
pub const DEFAULT_VERIFY_LAST: u32 = 8;
/// Largest accepted [`RecoveryOptions::verify_last`].
pub const MAX_VERIFY_LAST: u32 = 4096;
/// At most this many unknown entries are listed in [`RecoveryReport::unknown`].
pub const MAX_REPORTED_UNKNOWN: usize = 32;

/// What `open` does with corruption that is not a torn tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepairPolicy {
    /// Fail with `Error::CdxCorrupt` or `Error::I2Violation`; nothing on disk is changed. Default.
    #[default]
    Refuse,
    /// Truncate the CDX at the start of the first offending line (corrupt line, or line violating
    /// I2), dropping it and every later line, possibly acknowledged entries. Segments are never
    /// touched. Reported in `RecoveryReport::repair`. An explicit operator action, never the
    /// engine's default.
    TruncateAtCorruption,
}

/// Options of [`Collection::open`](crate::Collection::open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryOptions {
    /// Corruption policy.
    pub repair: RepairPolicy,
    /// Number of last scanned `Add` entries (in CDX order) whose stored bytes are read and
    /// digest-checked. 0 disables the deep check. At most [`MAX_VERIFY_LAST`].
    pub verify_last: u32,
    /// Ignore the checkpoint and scan the whole CDX (operator check). A partial scan does not
    /// re-read the checkpointed prefix, so bit rot in it is not seen by `open`; it is caught on
    /// read (digest check on every read) or by an open with `full_scan: true`. The header line is
    /// read and checked in every mode. Default false.
    pub full_scan: bool,
}

impl Default for RecoveryOptions {
    fn default() -> Self {
        RecoveryOptions {
            repair: RepairPolicy::Refuse,
            verify_last: DEFAULT_VERIFY_LAST,
            full_scan: false,
        }
    }
}

/// A torn CDX tail that was cut off: invalid bytes `[at, end)` with no valid line after them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TornTail {
    /// Offset of the first invalid byte (start of the torn line); the new CDX length unless a
    /// repair cut lower.
    pub at: u64,
    /// CDX length before recovery.
    pub end: u64,
}

/// Why a repair cut the CDX.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RepairReason {
    /// `CdxReader` reported `Corrupt` for the line at the cut.
    Corrupt(LineError),
    /// The line at the cut violates I2.
    I2(I2Violation),
}

/// A repair cut (only with `RepairPolicy::TruncateAtCorruption`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Repair {
    /// CDX offset of the first dropped line (= new CDX length).
    pub at: u64,
    /// CDX length before recovery.
    pub end: u64,
    /// Why.
    pub reason: RepairReason,
}

/// How the CDX was scanned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanMode {
    /// Lines `[from, end)` were scanned; `[header, from)` was trusted from the checkpoint.
    Partial {
        /// Offset of the first scanned line.
        from: u64,
    },
    /// Every line was scanned.
    Full {
        /// Why the checkpoint was not used.
        reason: FullScanReason,
    },
}

/// Why a full scan was done although a checkpoint could have been used.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FullScanReason {
    /// `RecoveryOptions::full_scan`.
    Forced,
    /// The checkpoint file is absent.
    NoCheckpoint,
    /// An I/O error other than `NotFound` while reading it.
    Unreadable,
    /// The checkpoint path exists but is not a regular file (reported as unknown, never written).
    NotAFile,
    /// Too large, bad JSON, wrong version, too many segments.
    Malformed,
    /// The file's generation is not the active one.
    Generation {
        /// Generation found in the file.
        found: u32,
    },
    /// The checkpoint offset is beyond the CDX length.
    BeyondCdx {
        /// Checkpoint offset.
        offset: u64,
        /// CDX length.
        cdx_len: u64,
    },
    /// The offset is below the header end, or the byte before it is not a newline.
    NotLineAligned {
        /// Checkpoint offset.
        offset: u64,
    },
    /// The checkpoint names a segment that does not exist.
    SegmentMissing {
        /// The segment.
        segment: u32,
    },
    /// A segment is shorter than the end the checkpoint recorded for it.
    SegmentShrunk {
        /// The segment.
        segment: u32,
        /// Its length now.
        len: u64,
        /// The recorded end.
        end: u64,
    },
}

/// What `open` found and did. Stable enough to log field by field. Libraries do not log; the
/// engine does.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveryReport {
    /// Active generation (manifest `active.id`).
    pub generation: u32,
    /// Number of segments of the active generation (`1..=segments` exist).
    pub segments: u32,
    /// Number of CDX lines kept (Add and Tombstone; the warcinfo entry included).
    pub entries: u64,
    /// CDX length after recovery: the offset where the next line will be written. A checkpoint
    /// `(generation, offset)` with `offset <= cdx_len` taken before this open stays valid for
    /// `CdxReader::resume` unless `repair` is `Some` and `offset > repair.at`.
    pub cdx_len: u64,
    /// Torn tail cut off, if any.
    pub cdx_torn: Option<TornTail>,
    /// Repair cut, if any (only with `TruncateAtCorruption`). If both are `Some`, the file was cut
    /// once, at `repair.at` (always `<= cdx_torn.at`).
    pub repair: Option<Repair>,
    /// Number of `Add` entries of the scanned range whose bytes were read and digest-checked.
    pub verified: u32,
    /// Number of `.tmp-*` entries removed.
    pub tmp_removed: usize,
    /// Files of non-active generations deleted, sorted by path.
    pub removed: Vec<PathBuf>,
    /// Unknown entries left in place (first `MAX_REPORTED_UNKNOWN` encountered, sorted by path).
    pub unknown: Vec<PathBuf>,
    /// Total number of unknown entries (may exceed `unknown.len()`).
    pub unknown_count: u64,
    /// How the CDX was scanned.
    pub scan: ScanMode,
    /// Offset of the checkpoint written by this open, if one was written.
    pub checkpoint_written: Option<u64>,
}

impl RecoveryReport {
    /// True iff nothing was repaired, cut, removed or swept (`cdx_torn`, `repair` are `None`,
    /// `removed` empty, `tmp_removed == 0`). Unknown entries, `scan` and `checkpoint_written` do
    /// not count.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.cdx_torn.is_none()
            && self.repair.is_none()
            && self.removed.is_empty()
            && self.tmp_removed == 0
    }
}

pub(crate) struct DirScan {
    pub(crate) leftovers: Vec<PathBuf>,
    pub(crate) unknown: Vec<PathBuf>,
    pub(crate) unknown_count: u64,
    /// A non-regular entry is named `scan.<active>.json`: the checkpoint can be neither read,
    /// written nor removed; it is reported as unknown and left alone.
    pub(crate) ck_blocked: bool,
}

/// Read-only classification of `dir`. The unknown list is the first
/// `MAX_REPORTED_UNKNOWN` entries encountered (so it depends on `read_dir` order), then sorted.
pub(crate) fn scan_dir(dir: &Path, active: Generation) -> Result<DirScan, Error> {
    let io_err = |op: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::Io { op, path, source }
    };
    let mut out = DirScan {
        leftovers: Vec::new(),
        unknown: Vec::new(),
        unknown_count: 0,
        ck_blocked: false,
    };
    let active_zst = matches!(active.codec, Codec::Zstd { .. });
    for entry in fs::read_dir(dir).map_err(io_err("read_dir", dir))? {
        let entry = entry.map_err(io_err("read_dir", dir))?;
        let path = entry.path();
        let ft = entry.file_type().map_err(io_err("stat", &path))?;
        // `Some(true)`: leftover candidate, `Some(false)`: keep silently, `None`: unknown.
        let verdict = match classify(&entry.file_name()) {
            FileClass::Manifest | FileClass::Reserved => Some(false),
            FileClass::Segment { generation, zst } => {
                if generation != active.id {
                    Some(true)
                } else if zst == active_zst {
                    Some(false)
                } else {
                    None
                }
            }
            FileClass::Checkpoint { generation } if generation == active.id && !ft.is_file() => {
                out.ck_blocked = true;
                None
            }
            FileClass::Cdx { generation } | FileClass::Checkpoint { generation } => {
                Some(generation != active.id)
            }
            FileClass::Unknown => None,
        };
        match verdict {
            Some(false) => {}
            Some(true) if !ft.is_dir() => out.leftovers.push(path),
            _ => {
                out.unknown_count = out
                    .unknown_count
                    .checked_add(1)
                    .ok_or(Error::Internal("unknown count"))?;
                if out.unknown.len() < MAX_REPORTED_UNKNOWN {
                    out.unknown.push(path);
                }
            }
        }
    }
    out.leftovers.sort();
    out.unknown.sort();
    Ok(out)
}

/// Cheap I2 check of one line against the segment lengths of the generation.
fn check_bounds(entry: &Entry<'_>, lens: &[u64]) -> Result<(), I2Violation> {
    let segments = u32::try_from(lens.len()).unwrap_or(u32::MAX);
    let len = entry
        .segment
        .checked_sub(1)
        .and_then(|i| usize::try_from(i).ok())
        .and_then(|i| lens.get(i));
    let Some(&segment_len) = len else {
        return Err(I2Violation::SegmentMissing {
            segment: entry.segment,
            segments,
        });
    };
    match entry.end() {
        Some(end) if end <= segment_len => Ok(()),
        _ => Err(I2Violation::PastEnd {
            segment: entry.segment,
            offset: entry.offset,
            raw_size: entry.raw_size,
            segment_len,
        }),
    }
}

/// An `Add` entry kept for the deep check (no strings).
struct Recent {
    ordinal: u64,
    cdx_offset: u64,
    loc: Locator,
    digest: u64,
}

struct CdxScan {
    /// CDX length before recovery.
    file_len: u64,
    /// End of the last valid line read.
    valid_len: u64,
    /// Lines kept (including the checkpointed prefix).
    entries: u64,
    torn: Option<TornTail>,
    cut: Option<Repair>,
    recent: VecDeque<Recent>,
    verified: u32,
    mode: ScanMode,
    /// Per segment, maximum end over the kept lines (prefix included).
    seg_ends: Vec<u64>,
    /// A line named a segment above `MAX_CHECKPOINT_SEGMENTS`: `seg_ends` is incomplete.
    seg_overflow: bool,
    /// Header end or checkpoint offset.
    scanned_from: u64,
}

fn bump_seg_end(seg_ends: &mut Vec<u64>, segment: u32, end: u64) -> bool {
    let Some(seg) = usize::try_from(segment)
        .ok()
        .filter(|s| (1..=MAX_CHECKPOINT_SEGMENTS).contains(s))
    else {
        return false;
    };
    if seg_ends.len() < seg {
        seg_ends.resize(seg, 0);
    }
    if let Some(e) = seg_ends.get_mut(seg - 1) {
        *e = (*e).max(end);
    }
    true
}

/// Checks a checkpoint against the segment lengths (stat only).
fn check_segments(ck: &CheckpointV1, lens: &[u64]) -> Result<(), FullScanReason> {
    for (i, &end) in ck.seg_ends.iter().enumerate() {
        let segment = u32::try_from(i.saturating_add(1)).unwrap_or(u32::MAX);
        match lens.get(i) {
            None => return Err(FullScanReason::SegmentMissing { segment }),
            Some(&len) if len < end => {
                return Err(FullScanReason::SegmentShrunk { segment, len, end });
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Checks that the checkpoint offset is a line boundary of the CDX.
fn check_alignment(
    file: &File,
    ck: CheckpointV1,
    header_len: u64,
    file_len: u64,
) -> Result<CheckpointV1, FullScanReason> {
    let offset = ck.cdx_offset;
    if offset > file_len {
        return Err(FullScanReason::BeyondCdx {
            offset,
            cdx_len: file_len,
        });
    }
    if offset < header_len {
        return Err(FullScanReason::NotLineAligned { offset });
    }
    if offset > header_len {
        let mut b = [0u8; 1];
        let pos = offset
            .checked_sub(1)
            .ok_or(FullScanReason::NotLineAligned { offset })?;
        file.read_exact_at(&mut b, pos)
            .map_err(|_| FullScanReason::Unreadable)?;
        if b != [b'\n'] {
            return Err(FullScanReason::NotLineAligned { offset });
        }
    }
    Ok(ck)
}

fn scan_cdx(
    path: &Path,
    generation: u32,
    lens: &[u64],
    recovery: &RecoveryOptions,
    ck: Result<CheckpointV1, FullScanReason>,
) -> Result<CdxScan, Error> {
    let io_err = |op: &'static str| {
        let path = path.to_path_buf();
        move |source| Error::Io { op, path, source }
    };
    let cdx_read = |source| Error::CdxRead {
        path: path.to_path_buf(),
        source,
    };
    let file = File::open(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            Error::CdxMissing {
                path: path.to_path_buf(),
            }
        } else {
            io_err("open")(e)
        }
    })?;
    let file_len = file.metadata().map_err(io_err("stat"))?.len();
    let r = CdxReader::open(BufReader::with_capacity(64 * 1024, file), DEFAULT_MAX_LINE)
        .map_err(cdx_read)?;
    if r.generation() != generation {
        return Err(Error::CdxGeneration {
            path: path.to_path_buf(),
            expected: generation,
            found: r.generation(),
        });
    }
    let header_len = r.offset();
    let mut br = r.into_inner();
    let checked = ck.and_then(|c| {
        check_alignment(br.get_ref(), c, header_len, file_len)
            .and_then(|c| check_segments(&c, lens).map(|()| c))
    });
    let (start, mode, mut entries, mut seg_ends) = match checked {
        Ok(c) => (
            c.cdx_offset,
            ScanMode::Partial { from: c.cdx_offset },
            c.entries,
            c.seg_ends,
        ),
        Err(reason) => (header_len, ScanMode::Full { reason }, 0, Vec::new()),
    };
    br.seek(SeekFrom::Start(start)).map_err(io_err("seek"))?;
    let mut r = CdxReader::resume(br, generation, start, DEFAULT_MAX_LINE).map_err(cdx_read)?;

    let cap = usize::try_from(recovery.verify_last).map_err(|_| Error::Internal("verify_last"))?;
    let mut recent = VecDeque::with_capacity(cap);
    let mut seg_overflow = false;
    let mut torn = None;
    let mut cut = None;
    let policy = recovery.repair;
    let valid_len = loop {
        match r.next_step() {
            Ok(Step::Entry { offset, entry }) => {
                if let Err(v) = check_bounds(&entry, lens) {
                    match policy {
                        RepairPolicy::Refuse => {
                            return Err(Error::I2Violation {
                                path: path.to_path_buf(),
                                at: offset,
                                violation: v,
                            });
                        }
                        RepairPolicy::TruncateAtCorruption => {
                            cut = Some(Repair {
                                at: offset,
                                end: file_len,
                                reason: RepairReason::I2(v),
                            });
                            break offset;
                        }
                    }
                }
                if let Some(end) = entry.end()
                    && !bump_seg_end(&mut seg_ends, entry.segment, end)
                {
                    seg_overflow = true;
                }
                if entry.kind == Kind::Add && cap > 0 {
                    if recent.len() == cap {
                        recent.pop_front();
                    }
                    recent.push_back(Recent {
                        ordinal: entries,
                        cdx_offset: offset,
                        loc: Locator {
                            segment: entry.segment,
                            offset: entry.offset,
                            len: entry.raw_size,
                        },
                        digest: entry.digest,
                    });
                }
                entries = entries.checked_add(1).ok_or(Error::Internal("entries"))?;
            }
            Ok(Step::End(Tail::Clean { end })) => break end,
            Ok(Step::End(Tail::Torn { at, end })) => {
                torn = Some(TornTail { at, end });
                break at;
            }
            Err(masstuffy_cdx::ReadError::Corrupt { at, source }) => match policy {
                RepairPolicy::Refuse => {
                    return Err(Error::CdxCorrupt {
                        path: path.to_path_buf(),
                        at,
                        end: file_len,
                        source,
                    });
                }
                RepairPolicy::TruncateAtCorruption => {
                    cut = Some(Repair {
                        at,
                        end: file_len,
                        reason: RepairReason::Corrupt(source),
                    });
                    break at;
                }
            },
            Err(other) => return Err(cdx_read(other)),
        }
    };
    Ok(CdxScan {
        file_len,
        valid_len,
        entries,
        torn,
        cut,
        recent,
        verified: 0,
        mode,
        seg_ends,
        seg_overflow,
        scanned_from: start,
    })
}

/// Deep I2 check: reads the stored bytes of the recent `Add` entries and compares digests.
fn verify_recent(
    snapshot: &Snapshot,
    path: &Path,
    out: &mut CdxScan,
    policy: RepairPolicy,
) -> Result<(), Error> {
    let mut buf = Vec::new();
    for rec in &out.recent {
        match snapshot.reader().read_at(&rec.loc, &mut buf) {
            Ok(()) => {}
            // Larger than this process's `max_stored_bytes`: cannot be verified with these options.
            Err(masstuffy_seglog::Error::ReadTooLarge { .. }) => continue,
            Err(e) => return Err(Error::Segment(e)),
        }
        let actual = masstuffy_cdx::digest(&buf);
        if actual != rec.digest {
            let v = I2Violation::DigestMismatch {
                segment: rec.loc.segment,
                offset: rec.loc.offset,
                raw_size: rec.loc.len,
                expected: rec.digest,
                actual,
            };
            match policy {
                RepairPolicy::Refuse => {
                    return Err(Error::I2Violation {
                        path: path.to_path_buf(),
                        at: rec.cdx_offset,
                        violation: v,
                    });
                }
                RepairPolicy::TruncateAtCorruption => {
                    // Every `recent` entry is below any scan cut, so this cut is the lowest.
                    out.cut = Some(Repair {
                        at: rec.cdx_offset,
                        end: out.file_len,
                        reason: RepairReason::I2(v),
                    });
                    out.entries = rec.ordinal;
                    break;
                }
            }
        }
        out.verified = out
            .verified
            .checked_add(1)
            .ok_or(Error::Internal("verified"))?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Number of phase-B CDX fsyncs of the scanned range (test observation). Thread-local on
    /// purpose: each test runs on its own thread and `open` is synchronous, so tests running in
    /// parallel never see each other's counts.
    static CDX_SYNCS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// What phase B does with `scan.<gen>.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CkAction {
    /// Leave the file as it is (still valid for its own prefix, or nothing to do).
    Keep,
    /// Write a checkpoint at the recovered length.
    Write,
    /// Unlink it: stale after a cut, or not replaceable by a complete one after a full scan.
    Remove,
}

fn checkpoint_action(cut: bool, full: bool, grew: bool, overflow: bool, blocked: bool) -> CkAction {
    if blocked {
        CkAction::Keep
    } else if cut {
        CkAction::Remove
    } else if overflow {
        // Never write one: `seg_ends` would miss a segment. A partial scan keeps the old one.
        if full {
            CkAction::Remove
        } else {
            CkAction::Keep
        }
    } else if full || grew {
        CkAction::Write
    } else {
        CkAction::Keep
    }
}

fn remove_checkpoint(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            op: "remove",
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// See the module docs. Returns the writer, the snapshot and the report.
pub(crate) fn open(
    dir: &Path,
    opts: &Options,
    recovery: &RecoveryOptions,
    resolver: &dyn DictResolver,
) -> Result<(Writer, Snapshot, RecoveryReport), Error> {
    // ---- Phase A: read only.
    if recovery.verify_last > MAX_VERIFY_LAST {
        return Err(Error::InvalidOptions("verify_last"));
    }
    let manifest = Manifest::load(dir)?;
    let active = manifest.active();
    let threshold = manifest.split_threshold();
    let ctx = Arc::new(GenContext::new(&manifest, opts, resolver)?);
    // The manifest may come from a rename that is not durable yet: make it durable before any
    // decision (which generation is a leftover) is based on it.
    masstuffy_durable::sync_dir(dir)?;
    let scan = scan_dir(dir, active)?;
    let naming = segment_naming(active.id, &active.codec)?;
    let appender = Appender::open(dir, naming, threshold)?;
    let snapshot = Snapshot::new(Arc::new(manifest), Arc::clone(&ctx), dir, opts)?;
    let segments = appender.current_segment();
    let mut lens: Vec<u64> = Vec::with_capacity(
        usize::try_from(segments).map_err(|_| Error::Internal("segment count"))?,
    );
    for n in 1..=segments {
        lens.push(snapshot.reader().len_of(n)?);
    }
    let cdx_path = dir.join(cdx_name(active.id));
    let ck = if recovery.full_scan {
        Err(FullScanReason::Forced)
    } else if scan.ck_blocked {
        Err(FullScanReason::NotAFile)
    } else {
        checkpoint::load(dir, active.id)
    };
    let mut out = scan_cdx(&cdx_path, active.id, &lens, recovery, ck)?;
    verify_recent(&snapshot, &cdx_path, &mut out, recovery.repair)?;

    // ---- Phase B: mutate.
    let target = out.cut.as_ref().map_or(out.valid_len, |r| r.at);
    if target > out.file_len {
        return Err(Error::Internal("cut past end"));
    }
    if target < out.file_len {
        let io_err = |op: &'static str| {
            let path = cdx_path.clone();
            move |source| Error::Io { op, path, source }
        };
        let f = OpenOptions::new()
            .write(true)
            .open(&cdx_path)
            .map_err(io_err("open"))?;
        f.set_len(target).map_err(io_err("truncate"))?;
        f.sync_data().map_err(io_err("fsync"))?;
    }

    // Lines scanned past the checkpoint may have been written, not fsynced, by a poisoned writer
    // of this boot: make them durable before anything (checkpoint, `Writer::checkpoint`) relies on
    // them. On a clean file this is one nearly free fdatasync.
    if target > out.scanned_from {
        let f = File::open(&cdx_path).map_err(|source| Error::Io {
            op: "open",
            path: cdx_path.clone(),
            source,
        })?;
        f.sync_data().map_err(|source| Error::Io {
            op: "fsync",
            path: cdx_path.clone(),
            source,
        })?;
        #[cfg(test)]
        CDX_SYNCS.with(|c| c.set(c.get() + 1));
    }

    // Checkpoint maintenance, before the Writer exists.
    let ck_path = dir.join(checkpoint_name(active.id));
    let full = matches!(out.mode, ScanMode::Full { .. });
    let grew = target.saturating_sub(out.scanned_from) >= CHECKPOINT_MIN_GROWTH;
    let mut checkpoint_written = None;
    let mut action = checkpoint_action(
        out.cut.is_some(),
        full,
        grew,
        out.seg_overflow,
        scan.ck_blocked,
    );
    if action == CkAction::Write {
        if checkpoint::write(dir, active.id, target, out.entries, &out.seg_ends)? {
            checkpoint_written = Some(target);
        } else if full {
            // Too large to store: a stale file must not survive a full scan.
            action = CkAction::Remove;
        }
    }
    if action == CkAction::Remove {
        remove_checkpoint(&ck_path)?;
        masstuffy_durable::sync_dir(dir)?;
    }

    let tmp_removed = masstuffy_durable::sweep_tmp(dir)?;
    let mut removed = Vec::with_capacity(scan.leftovers.len());
    for p in scan.leftovers {
        match fs::remove_file(&p) {
            Ok(()) => removed.push(p),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Io {
                    op: "remove",
                    path: p,
                    source,
                });
            }
        }
    }
    if !removed.is_empty() {
        masstuffy_durable::sync_dir(dir)?;
    }

    let file = OpenOptions::new()
        .append(true)
        .open(&cdx_path)
        .map_err(|source| Error::Io {
            op: "open",
            path: cdx_path.clone(),
            source,
        })?;
    let cdx = CdxWriter::append(file, DEFAULT_MAX_LINE);
    let accepted = match out.mode {
        ScanMode::Partial { from } => from,
        ScanMode::Full { .. } => 0,
    };
    let state = ScanState {
        cdx_len: target,
        entries: out.entries,
        seg_ends: out.seg_ends,
        valid: out.cut.is_none() && !out.seg_overflow && !scan.ck_blocked,
    };
    let mut writer = Writer::from_parts(ctx, appender, cdx, cdx_path, state);
    writer.last_checkpoint = checkpoint_written.unwrap_or(accepted);
    let report = RecoveryReport {
        generation: active.id,
        segments,
        entries: out.entries,
        cdx_len: target,
        cdx_torn: out.torn,
        repair: out.cut,
        verified: out.verified,
        tmp_removed,
        removed,
        unknown: scan.unknown,
        unknown_count: scan.unknown_count,
        scan: out.mode,
        checkpoint_written,
    };
    Ok((writer, snapshot, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ReadError;
    use crate::manifest::Manifest;
    use crate::snapshot::ReadCtx;
    use crate::testutil::{
        MapResolver, TestDir, append_cdx_line, cdx_lines, clone_dir, commit_batch, create_none,
        create_with, create_zstd, creators, file_len, flip_byte, listing, names, offsets, open_ok,
        open_with, read_cdx_all, record, seg1, truncate, with_check, zstd_codec,
    };
    use crate::writer::FailPoint;
    use crate::{Collection, DEFAULT_SPLIT_THRESHOLD};
    use masstuffy_seglog::Error as SegError;
    use std::num::NonZeroU64;

    fn refuse_full() -> RecoveryOptions {
        RecoveryOptions {
            full_scan: true,
            ..RecoveryOptions::default()
        }
    }

    fn repair_opts() -> RecoveryOptions {
        RecoveryOptions {
            repair: RepairPolicy::TruncateAtCorruption,
            ..RecoveryOptions::default()
        }
    }

    fn read_all(c: &Collection, es: &[Entry<'static>]) {
        let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
        for e in es {
            c.snapshot().read(e, &mut ctx, &mut buf).unwrap();
        }
    }

    // ---- Fault-injection fixture.

    struct Base {
        d: TestDir,
        dir: PathBuf,
        zstd: bool,
        ck: bool,
        seg_a: u64,
        seg_b: u64,
        cdx_a: u64,
        cdx_b: u64,
        b_lines: Vec<(u64, u64)>,
        b_bounds: Vec<u64>,
        expected: Vec<Entry<'static>>,
        inputs: Vec<Option<Vec<u8>>>,
    }

    impl Base {
        fn new(zstd: bool, ck: bool, threshold: NonZeroU64) -> Base {
            let d = TestDir::new();
            let dir = d.path().join("base");
            let codec = if zstd { zstd_codec() } else { Codec::None };
            let (mut c, wi) = create_with(&dir, codec, threshold, &Options::default());
            let mut p = c.preparer().unwrap();
            let mut expected = vec![wi];
            let mut inputs = vec![None];
            let a = vec![record(1, 200), record(2, 300)];
            inputs.extend(a.iter().map(|r| Some(r.as_bytes().to_vec())));
            expected.extend(commit_batch(&mut c, &mut p, a));
            let last_a = expected.last().unwrap().clone();
            let seg_a = file_len(&seg1(&dir, zstd, last_a.segment));
            let cdx_a = file_len(&dir.join("index.1.cdx"));
            if ck {
                assert!(c.checkpoint().unwrap());
            }
            let b: Vec<_> = (3..=6).map(|i| record(i, 150 + 50 * i)).collect();
            inputs.extend(b.iter().map(|r| Some(r.as_bytes().to_vec())));
            let eb = commit_batch(&mut c, &mut p, b);
            let seg_b = file_len(&seg1(&dir, zstd, eb[0].segment));
            let cdx_b = file_len(&dir.join("index.1.cdx"));
            let mut b_bounds = Vec::new();
            for e in &eb {
                b_bounds.push(e.offset);
                b_bounds.push(e.end().unwrap());
            }
            expected.extend(eb);
            let b_lines = cdx_lines(&dir, 1)
                .into_iter()
                .skip(3)
                .map(|(s, e, _)| (s, e))
                .collect();
            drop(c);
            Base {
                d,
                dir,
                zstd,
                ck,
                seg_a,
                seg_b,
                cdx_a,
                cdx_b,
                b_lines,
                b_bounds,
                expected,
                inputs,
            }
        }

        fn fresh(&self) -> PathBuf {
            let case = self.d.path().join("case");
            let _ = fs::remove_dir_all(&case);
            clone_dir(&self.dir, &case);
            case
        }

        fn seg(&self, case: &Path) -> PathBuf {
            seg1(case, self.zstd, self.expected[3].segment)
        }

        /// Opens `case`, checks the recovered state, optionally commits C and reopens.
        fn run(&self, case: &Path, k: usize, torn: bool, extra: bool) -> RecoveryReport {
            let (mut c, rep) = open_ok(case);
            let ctx = format!("k={k} torn={torn}");
            assert_eq!(rep.cdx_torn.is_some(), torn, "{ctx}");
            assert!(rep.repair.is_none(), "{ctx}");
            assert_eq!(rep.entries, u64::try_from(k).unwrap(), "{ctx}");
            assert_eq!(rep.cdx_len, file_len(&case.join("index.1.cdx")), "{ctx}");
            if self.ck {
                assert_eq!(rep.scan, ScanMode::Partial { from: self.cdx_a }, "{ctx}");
            }
            let (es, tail) = read_cdx_all(case, 1);
            assert!(matches!(tail, Tail::Clean { .. }), "{ctx}");
            assert_eq!(es, self.expected[..k], "{ctx}");
            let (mut rctx, mut buf) = (ReadCtx::new(), Vec::new());
            for (e, input) in es.iter().zip(&self.inputs) {
                let rec = c.snapshot().read(e, &mut rctx, &mut buf).unwrap();
                if let Some(input) = input {
                    assert_eq!(rec.as_bytes(), &input[..], "{ctx}");
                }
            }
            if extra {
                let seg_before = file_len(&seg1(case, self.zstd, rep.segments));
                let mut p = c.preparer().unwrap();
                let r = record(100, 80);
                let want = r.as_bytes().to_vec();
                let ce = commit_batch(&mut c, &mut p, vec![r]);
                assert_eq!(ce[0].segment, rep.segments, "{ctx}");
                assert_eq!(ce[0].offset, seg_before, "{ctx}");
                let rec = c.snapshot().read(&ce[0], &mut rctx, &mut buf).unwrap();
                assert_eq!(rec.as_bytes(), &want[..]);
                drop(p);
                drop(c);
                let (_c2, rep2) = open_ok(case);
                assert!(rep2.is_clean(), "{ctx}: {rep2:?}");
                assert_eq!(rep2.entries, u64::try_from(k + 1).unwrap(), "{ctx}");
            }
            rep
        }

        fn lines_before(&self, len: u64) -> usize {
            self.b_lines.iter().filter(|&&(_, e)| e <= len).count()
        }
    }

    fn bases(ck: bool) -> Vec<(u64, Base)> {
        vec![
            (97, Base::new(false, ck, DEFAULT_SPLIT_THRESHOLD)),
            (251, Base::new(true, ck, DEFAULT_SPLIT_THRESHOLD)),
        ]
    }

    #[test]
    fn crash_during_data_append() {
        for ck in [false, true] {
            for (stride, b) in bases(ck) {
                for (i, s) in offsets(b.seg_a, b.seg_b, &b.b_bounds, stride)
                    .into_iter()
                    .enumerate()
                {
                    let case = b.fresh();
                    truncate(&b.seg(&case), s);
                    truncate(&case.join("index.1.cdx"), b.cdx_a);
                    let extra = i % 25 == 0 || b.b_bounds.contains(&s);
                    b.run(&case, 3, false, extra);
                }
            }
        }
    }

    #[test]
    fn crash_during_cdx_write() {
        for ck in [false, true] {
            for (_, b) in bases(ck) {
                // With a checkpoint the open is cheap: every byte; otherwise a stride.
                let stride = match (ck, b.zstd) {
                    (true, false) => 1,
                    (true, true) => 13,
                    (false, false) => 11,
                    (false, true) => 29,
                };
                let ends: Vec<u64> = b.b_lines.iter().flat_map(|&(s, e)| [s, e]).collect();
                for (i, l) in offsets(b.cdx_a, b.cdx_b, &ends, stride)
                    .into_iter()
                    .enumerate()
                {
                    let case = b.fresh();
                    truncate(&case.join("index.1.cdx"), l);
                    let k = 3 + b.lines_before(l);
                    let torn = !(l == b.cdx_a || b.b_lines.iter().any(|&(_, e)| e == l));
                    b.run(&case, k, torn, i % 40 == 0);
                }
            }
        }
    }

    #[test]
    fn zero_filled_and_garbage_cdx_tail() {
        for ck in [false, true] {
            let b = Base::new(false, ck, DEFAULT_SPLIT_THRESHOLD);
            let ends: Vec<u64> = b.b_lines.iter().flat_map(|&(s, e)| [s, e]).collect();
            let stride = if ck { 7 } else { 17 };
            for (i, z) in offsets(b.cdx_a, b.cdx_b, &ends, stride)
                .into_iter()
                .enumerate()
            {
                for fill in [0u8, 0xff] {
                    if fill != 0 && i % 9 != 0 {
                        continue;
                    }
                    let case = b.fresh();
                    let path = case.join("index.1.cdx");
                    let mut bytes = fs::read(&path).unwrap();
                    for x in bytes.iter_mut().skip(usize::try_from(z).unwrap()) {
                        *x = fill;
                    }
                    fs::write(&path, bytes).unwrap();
                    let k = 3 + b.lines_before(z);
                    b.run(&case, k, z < b.cdx_b, i % 30 == 0);
                }
            }
        }
    }

    #[test]
    fn crash_with_rollover() {
        for ck in [false, true] {
            let b = Base::new(false, ck, NonZeroU64::MIN);
            assert_eq!(b.expected[3].segment, 3);
            for (state, want_segments, c_segment) in [(0, 2, 3), (1, 3, 3), (2, 3, 4)] {
                let case = b.fresh();
                truncate(&case.join("index.1.cdx"), b.cdx_a);
                let seg3 = seg1(&case, false, 3);
                match state {
                    0 => fs::remove_file(&seg3).unwrap(),
                    1 => truncate(&seg3, 0),
                    _ => truncate(&seg3, b.seg_b / 2),
                }
                let (mut c, rep) = open_ok(&case);
                assert_eq!(rep.segments, want_segments);
                assert_eq!(rep.entries, 3);
                let mut p = c.preparer().unwrap();
                let r = record(100, 80);
                let want = r.as_bytes().to_vec();
                let ce = commit_batch(&mut c, &mut p, vec![r]);
                assert_eq!(ce[0].segment, c_segment);
                let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
                let rec = c.snapshot().read(&ce[0], &mut ctx, &mut buf).unwrap();
                assert_eq!(rec.as_bytes(), &want[..]);
                drop(p);
                drop(c);
                let (_c, rep) = open_ok(&case);
                assert!(rep.is_clean());
                assert_eq!(rep.entries, 4);
            }
        }
    }

    #[test]
    fn reopen_after_fail_points() {
        for (_, create) in creators() {
            for point in [FailPoint::BeforeCdxWrite, FailPoint::BeforeCdxSync] {
                let d = TestDir::new();
                let dir = d.sub();
                let (mut c, _) = create(&dir);
                let mut p = c.preparer().unwrap();
                let good = commit_batch(&mut c, &mut p, vec![record(1, 20)]);
                c.writer().fail = Some(point);
                let batch = vec![p.prepare(record(2, 20)).unwrap()];
                assert!(c.writer().commit(&batch).is_err());
                assert!(c.writer().is_poisoned());
                assert!(c.checkpoint().is_err());
                drop(p);
                drop(c);
                let (mut c, rep) = open_ok(&dir);
                assert!(rep.is_clean(), "{rep:?}");
                let present = matches!(point, FailPoint::BeforeCdxSync);
                assert_eq!(rep.entries, if present { 3 } else { 2 });
                read_all(&c, &good);
                let (es, _) = read_cdx_all(&dir, 1);
                assert_eq!(es.len(), if present { 3 } else { 2 });
                if present {
                    read_all(&c, &es);
                }
                let mut p = c.preparer().unwrap();
                let again = vec![p.prepare(record(2, 20)).unwrap()];
                assert_eq!(c.writer().commit(&again).unwrap().len(), 1);
                drop(p);
                drop(c);
                let (_c, rep) = open_ok(&dir);
                assert!(rep.is_clean());
            }
        }
    }

    // ---- Corruption.

    fn collection_with_singles(zstd: bool, n: usize) -> (TestDir, PathBuf) {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = if zstd {
            create_zstd(&dir)
        } else {
            create_none(&dir)
        };
        let mut p = c.preparer().unwrap();
        for i in 1..=n {
            commit_batch(&mut c, &mut p, vec![record(i, 100)]);
        }
        (d, dir)
    }

    #[test]
    fn corrupt_mid_cdx_refused_then_repaired() {
        for zstd in [false, true] {
            let (_d, dir) = collection_with_singles(zstd, 4);
            let lines = cdx_lines(&dir, 1);
            let (at, end_of_line) = (lines[1].0, lines[1].1);
            flip_byte(&dir.join("index.1.cdx"), at + (end_of_line - at) / 2);
            fs::write(dir.join(".tmp-1-1"), b"x").unwrap();
            let before = listing(&dir);
            let len = file_len(&dir.join("index.1.cdx"));
            match open_with(&dir, &RecoveryOptions::default()) {
                Err(Error::CdxCorrupt {
                    at: a,
                    end,
                    source: LineError::Checksum,
                    ..
                }) => assert_eq!((a, end), (at, len)),
                other => panic!("{:?}", other.map(|_| ())),
            }
            assert_eq!(listing(&dir), before);
            let (c, rep) = open_with(&dir, &repair_opts()).unwrap();
            assert_eq!(
                rep.repair,
                Some(Repair {
                    at,
                    end: len,
                    reason: RepairReason::Corrupt(LineError::Checksum)
                })
            );
            assert_eq!(rep.entries, 1);
            assert_eq!(file_len(&dir.join("index.1.cdx")), at);
            assert!(!dir.join(".tmp-1-1").exists());
            let (es, _) = read_cdx_all(&dir, 1);
            read_all(&c, &es);
            drop(c);
            let (_c, rep) = open_ok(&dir);
            assert!(rep.is_clean());
            assert_eq!(rep.entries, 1);
        }
    }

    #[test]
    fn checksum_valid_garbage_at_tail_is_corrupt() {
        let (_d, dir) = collection_with_singles(false, 2);
        let path = dir.join("index.1.cdx");
        let at = file_len(&path);
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut f, &with_check(b"garbage")).unwrap();
        drop(f);
        assert!(matches!(
            open_with(&dir, &RecoveryOptions::default()),
            Err(Error::CdxCorrupt {
                source: LineError::InvalidKind,
                ..
            })
        ));
        let (_c, rep) = open_with(&dir, &repair_opts()).unwrap();
        assert_eq!(rep.repair.as_ref().map(|r| r.at), Some(at));
        assert_eq!(rep.entries, 3);
        assert_eq!(rep.cdx_torn, None);
    }

    #[test]
    fn i2_violation_past_end() {
        for zstd in [false, true] {
            let (_d, dir) = collection_with_singles(zstd, 2);
            let path = dir.join("index.1.cdx");
            let seg_len = file_len(&seg1(&dir, zstd, 1));
            let at = file_len(&path);
            let mut e = cdx_lines(&dir, 1).pop().unwrap().2;
            e.offset = seg_len;
            append_cdx_line(&path, &e);
            let before = listing(&dir);
            match open_with(&dir, &RecoveryOptions::default()) {
                Err(Error::I2Violation {
                    at: a,
                    violation:
                        I2Violation::PastEnd {
                            segment: 1,
                            segment_len,
                            ..
                        },
                    ..
                }) => assert_eq!((a, segment_len), (at, seg_len)),
                other => panic!("{:?}", other.map(|_| ())),
            }
            assert_eq!(listing(&dir), before);
            let (mut c, rep) = open_with(&dir, &repair_opts()).unwrap();
            let r = rep.repair.clone().unwrap();
            assert_eq!(r.at, at);
            assert!(matches!(
                r.reason,
                RepairReason::I2(I2Violation::PastEnd { .. })
            ));
            assert_eq!(file_len(&path), at);
            let mut p = c.preparer().unwrap();
            let es = commit_batch(&mut c, &mut p, vec![record(50, 60)]);
            read_all(&c, &es);
        }
    }

    #[test]
    fn i2_violation_missing_segment() {
        let (_d, dir) = collection_with_singles(false, 1);
        let mut e = cdx_lines(&dir, 1).pop().unwrap().2;
        e.segment = 2;
        append_cdx_line(&dir.join("index.1.cdx"), &e);
        assert!(matches!(
            open_with(&dir, &RecoveryOptions::default()),
            Err(Error::I2Violation {
                violation: I2Violation::SegmentMissing {
                    segment: 2,
                    segments: 1
                },
                ..
            })
        ));
    }

    #[test]
    fn i2_violation_after_valid_lines_drops_later_lines() {
        let (_d, dir) = collection_with_singles(false, 2);
        let path = dir.join("index.1.cdx");
        let at = file_len(&path);
        let lines = cdx_lines(&dir, 1);
        let mut bad = lines[2].2.clone();
        bad.offset = file_len(&seg1(&dir, false, 1));
        append_cdx_line(&path, &bad);
        append_cdx_line(&path, &lines[1].2);
        append_cdx_line(&path, &lines[2].2);
        let (_c, rep) = open_with(&dir, &repair_opts()).unwrap();
        assert_eq!(rep.repair.map(|r| r.at), Some(at));
        assert_eq!(rep.entries, 3);
        assert_eq!(file_len(&path), at);
    }

    #[test]
    fn deep_check_digest_mismatch() {
        for zstd in [false, true] {
            let (d, dir) = collection_with_singles(zstd, 3);
            let lines = cdx_lines(&dir, 1);
            let last = &lines[3];
            flip_byte(&seg1(&dir, zstd, 1), last.2.offset + last.2.raw_size / 2);
            let variant = |n: &str| {
                let p = d.path().join(n);
                clone_dir(&dir, &p);
                p
            };
            match open_with(&variant("a"), &RecoveryOptions::default()) {
                Err(Error::I2Violation {
                    at,
                    violation: I2Violation::DigestMismatch { .. },
                    ..
                }) => assert_eq!(at, last.0),
                other => panic!("{:?}", other.map(|_| ())),
            }
            let opts = RecoveryOptions {
                verify_last: 0,
                ..RecoveryOptions::default()
            };
            let (c, rep) = open_with(&variant("b"), &opts).unwrap();
            assert_eq!(rep.verified, 0);
            let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
            assert!(matches!(
                c.snapshot().read(&last.2, &mut ctx, &mut buf),
                Err(ReadError::DigestMismatch { .. })
            ));
            let (_c, rep) = open_with(&variant("c"), &repair_opts()).unwrap();
            assert_eq!(rep.repair.as_ref().map(|r| r.at), Some(last.0));
            assert_eq!(rep.entries, 3);
            assert_eq!(rep.verified, 3);

            // Outside the window: not seen (documented limit).
            let first = &lines[1];
            let dir2 = variant("d");
            // Undo the last flip, corrupt the first record instead.
            flip_byte(&seg1(&dir2, zstd, 1), last.2.offset + last.2.raw_size / 2);
            flip_byte(&seg1(&dir2, zstd, 1), first.2.offset + first.2.raw_size / 2);
            let opts = RecoveryOptions {
                verify_last: 2,
                ..RecoveryOptions::default()
            };
            let (_c, rep) = open_with(&dir2, &opts).unwrap();
            assert_eq!(rep.verified, 2);
        }
    }

    #[test]
    fn verify_last_counts_and_limit() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = create_none(&dir);
        let mut p = c.preparer().unwrap();
        commit_batch(&mut c, &mut p, (1..=20).map(|i| record(i, 50)).collect());
        drop((p, c));
        for (n, want) in [(8, 8), (100, 21)] {
            let o = RecoveryOptions {
                verify_last: n,
                ..refuse_full()
            };
            let (_c, rep) = open_with(&dir, &o).unwrap();
            assert_eq!(rep.verified, want);
        }
        let o = RecoveryOptions {
            verify_last: MAX_VERIFY_LAST + 1,
            ..RecoveryOptions::default()
        };
        assert!(matches!(
            open_with(&dir, &o),
            Err(Error::InvalidOptions("verify_last"))
        ));

        // A stored record above this process's `max_stored_bytes` is skipped.
        let (mut c, _) = open_ok(&dir);
        let mut p = c.preparer().unwrap();
        commit_batch(&mut c, &mut p, vec![record(99, 300 * 1024)]);
        drop((p, c));
        let small = Options {
            max_record_bytes: 1024,
            ..Options::default()
        };
        let (_c, rep) =
            Collection::open(&dir, &small, &refuse_full(), &MapResolver::standard()).unwrap();
        assert_eq!(rep.verified, 7);
        assert_eq!(rep.entries, 22);
    }

    // ---- Directory contents.

    #[test]
    fn leftover_generations_removed() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (c, _) = create(&dir);
            drop(c);
            let touch = |n: &str| fs::write(dir.join(n), b"x").unwrap();
            let leftovers = [
                "seg.2.1.warc",
                "seg.2.2.warc.zst",
                "index.2.cdx",
                "seg.7.1.warc",
                "index.9.cdx",
            ];
            for n in leftovers {
                touch(n);
            }
            let mut unknown = vec![
                "seg.01.1.warc",
                "index.0.cdx",
                "seg.1.1.warc.bak",
                "notes.txt",
                "seg.3.1.warc",
            ];
            fs::remove_file(dir.join("seg.3.1.warc")).ok();
            fs::create_dir(dir.join("seg.3.1.warc")).unwrap();
            for n in &unknown {
                if *n != "seg.3.1.warc" {
                    touch(n);
                }
            }
            let wrong = if name == "none" {
                "seg.1.1.warc.zst"
            } else {
                "seg.1.1.warc"
            };
            if name == "none" {
                touch(wrong);
            } else {
                // The active segment of a Zstd collection is `.warc.zst`; a `.warc` is foreign.
                touch(wrong);
            }
            unknown.push(wrong);
            touch(".tmp-9-9");
            touch(".lock");
            let (c, rep) = open_ok(&dir);
            let mut want_removed: Vec<PathBuf> = leftovers.iter().map(|n| dir.join(n)).collect();
            want_removed.sort();
            let mut want_unknown: Vec<PathBuf> = unknown.iter().map(|n| dir.join(n)).collect();
            want_unknown.sort();
            assert_eq!(rep.removed, want_removed);
            assert_eq!(rep.unknown, want_unknown);
            assert_eq!(rep.unknown_count, 6);
            assert_eq!(rep.tmp_removed, 1);
            let ext = if name == "none" { "warc" } else { "warc.zst" };
            let mut want_names: Vec<String> = unknown.iter().map(|s| (*s).to_owned()).collect();
            want_names.extend(
                [
                    "manifest.json",
                    "index.1.cdx",
                    "scan.1.json",
                    ".lock",
                    &format!("seg.1.1.{ext}"),
                ]
                .map(str::to_owned),
            );
            want_names.sort();
            want_names.dedup();
            assert_eq!(names(&dir), want_names);
            drop(c);
            let (_c, rep) = open_ok(&dir);
            assert!(rep.removed.is_empty());
            assert_eq!(rep.unknown_count, 6);
            assert!(rep.is_clean());
        }
    }

    fn simulate_rebuild(dir: &Path, c: &Collection, switch: bool) {
        let m = c.manifest();
        let codec = m.active().codec;
        let a =
            Appender::create(dir, segment_naming(2, &codec).unwrap(), m.split_threshold()).unwrap();
        drop(a);
        crate::layout::create_cdx(dir, 2).unwrap();
        if switch {
            let m2 = Manifest::new(
                m.uuid(),
                m.slug(),
                m.split_threshold(),
                Generation { id: 2, codec },
                3,
            )
            .unwrap();
            let mut json = m2.to_json().unwrap();
            json.push(b'\n');
            masstuffy_durable::atomic_write(dir, "manifest.json", &json).unwrap();
        }
    }

    #[test]
    fn old_generation_removed_after_switch() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = create_none(&dir);
        let mut p = c.preparer().unwrap();
        commit_batch(&mut c, &mut p, vec![record(1, 100)]);
        c.checkpoint().unwrap();
        simulate_rebuild(&dir, &c, true);
        drop((p, c));
        let (mut c, rep) = open_ok(&dir);
        assert_eq!((rep.generation, rep.entries), (2, 0));
        let mut got: Vec<String> = rep
            .removed
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, ["index.1.cdx", "scan.1.json", "seg.1.1.warc"]);
        let mut p = c.preparer().unwrap();
        let es = commit_batch(&mut c, &mut p, vec![record(2, 100)]);
        read_all(&c, &es);
        drop((p, c));
        let (_c, rep) = open_ok(&dir);
        assert!(rep.is_clean());
        assert_eq!(rep.entries, 1);

        // Aborted rebuild: manifest still says generation 1.
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, wi) = create_none(&dir);
        let mut p = c.preparer().unwrap();
        let mut es = commit_batch(&mut c, &mut p, vec![record(1, 100)]);
        es.push(wi);
        simulate_rebuild(&dir, &c, false);
        drop((p, c));
        let (c, rep) = open_ok(&dir);
        assert_eq!((rep.generation, rep.entries), (1, 2));
        assert_eq!(rep.removed.len(), 2);
        read_all(&c, &es);
        assert!(!dir.join("index.2.cdx").exists());
    }

    #[test]
    fn refused_open_keeps_leftovers() {
        let (_d, dir) = collection_with_singles(false, 3);
        let lines = cdx_lines(&dir, 1);
        flip_byte(&dir.join("index.1.cdx"), lines[1].0 + 5);
        fs::write(dir.join("index.2.cdx"), b"x").unwrap();
        assert!(open_with(&dir, &RecoveryOptions::default()).is_err());
        assert!(dir.join("index.2.cdx").exists());
    }

    #[test]
    fn aborted_create_manifest_missing() {
        let d = TestDir::new();
        let (c, _) = create_none(&d.sub());
        drop(c);
        let dir = d.path().join("aborted");
        clone_dir(&d.sub(), &dir);
        fs::remove_file(dir.join("manifest.json")).unwrap();
        let before = listing(&dir);
        assert!(matches!(
            open_with(&dir, &RecoveryOptions::default()),
            Err(Error::ManifestMissing { .. })
        ));
        assert_eq!(listing(&dir), before);
        let empty = d.path().join("empty");
        fs::create_dir(&empty).unwrap();
        assert!(matches!(
            open_with(&empty, &RecoveryOptions::default()),
            Err(Error::ManifestMissing { .. })
        ));
    }

    #[test]
    fn structural_errors() {
        let d = TestDir::new();
        let (mut c, _) = create_none(&d.sub());
        let mut p = c.preparer().unwrap();
        commit_batch(&mut c, &mut p, vec![record(1, 100)]);
        drop((p, c));
        let variant = |n: &str| {
            let p = d.path().join(n);
            clone_dir(&d.sub(), &p);
            // A torn tail and a leftover, which must stay when the open fails.
            fs::write(p.join("index.2.cdx"), b"x").unwrap();
            p
        };
        let expect_unchanged = |p: &Path, f: &dyn Fn(Error) -> bool| {
            let before = listing(p);
            let e = open_with(p, &RecoveryOptions::default()).err().unwrap();
            assert!(f(e));
            assert_eq!(listing(p), before);
        };
        let p = variant("a");
        fs::remove_file(p.join("index.1.cdx")).unwrap();
        expect_unchanged(&p, &|e| matches!(e, Error::CdxMissing { .. }));
        let p = variant("b");
        truncate(&p.join("index.1.cdx"), 0);
        expect_unchanged(&p, &|e| {
            matches!(
                e,
                Error::CdxRead {
                    source: masstuffy_cdx::ReadError::MissingHeader,
                    ..
                }
            )
        });
        let p = variant("c");
        fs::write(p.join("index.1.cdx"), b"#masstuffy-cdx/1 gen=2\n").unwrap();
        expect_unchanged(&p, &|e| {
            matches!(
                e,
                Error::CdxGeneration {
                    expected: 1,
                    found: 2,
                    ..
                }
            )
        });
        let p = variant("e");
        fs::remove_file(p.join("seg.1.1.warc")).unwrap();
        expect_unchanged(&p, &|e| {
            matches!(e, Error::Segment(SegError::NoSegments { .. }))
        });

        // Gap: three segments, the middle one deleted.
        let dg = TestDir::new();
        let (mut c, _) = create_with(&dg.sub(), Codec::None, NonZeroU64::MIN, &Options::default());
        let mut p = c.preparer().unwrap();
        commit_batch(&mut c, &mut p, vec![record(1, 100)]);
        commit_batch(&mut c, &mut p, vec![record(2, 100)]);
        drop((p, c));
        assert!(seg1(&dg.sub(), false, 3).exists());
        fs::remove_file(seg1(&dg.sub(), false, 2)).unwrap();
        expect_unchanged(&dg.sub(), &|e| {
            matches!(e, Error::Segment(SegError::Gap { .. }))
        });

        // Missing dictionary: nothing touched (torn tail and leftover stay).
        let dz = TestDir::new();
        let (mut c, _) = create_zstd(&dz.sub());
        let mut p = c.preparer().unwrap();
        commit_batch(&mut c, &mut p, vec![record(1, 100)]);
        drop((p, c));
        fs::write(dz.sub().join("index.2.cdx"), b"x").unwrap();
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(dz.sub().join("index.1.cdx"))
            .unwrap();
        std::io::Write::write_all(&mut f, b"A 1 2 3").unwrap();
        drop(f);
        let before = listing(&dz.sub());
        let r = Collection::open(
            &dz.sub(),
            &Options::default(),
            &RecoveryOptions::default(),
            &MapResolver::default(),
        );
        assert!(matches!(r, Err(Error::Resolve(_))));
        assert_eq!(listing(&dz.sub()), before);
    }

    #[test]
    fn open_idempotent() {
        let (_d, dir) = collection_with_singles(false, 3);
        fs::write(dir.join("index.4.cdx"), b"x").unwrap();
        fs::write(dir.join(".tmp-5-5"), b"x").unwrap();
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(dir.join("index.1.cdx"))
            .unwrap();
        std::io::Write::write_all(&mut f, b"A 1 2 3").unwrap();
        drop(f);
        let (c, r1) = open_ok(&dir);
        assert!(!r1.is_clean());
        drop(c);
        let (c, r2) = open_ok(&dir);
        assert!(r2.is_clean());
        assert_eq!((r2.entries, r2.cdx_len), (r1.entries, r1.cdx_len));
        drop(c);
        let (_c, r3) = open_ok(&dir);
        assert_eq!(r2, r3);
    }

    #[test]
    fn open_commit_reopen() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (c, wi) = create(&dir);
            drop(c);
            let (mut c, rep) = open_ok(&dir);
            assert!(rep.is_clean());
            let mut p = c.preparer().unwrap();
            let mut all = vec![(wi, None)];
            let seg_len = file_len(&seg1(&dir, name == "zstd", 1));
            for b in 0..3 {
                let recs: Vec<_> = (0..2).map(|k| record(b * 10 + k, 100 + k)).collect();
                let inputs: Vec<_> = recs.iter().map(|r| r.as_bytes().to_vec()).collect();
                let es = commit_batch(&mut c, &mut p, recs);
                if b == 0 {
                    assert_eq!(es[0].offset, seg_len);
                }
                all.extend(es.into_iter().zip(inputs.into_iter().map(Some)));
            }
            drop((p, c));
            let (c, rep) = open_ok(&dir);
            assert_eq!(rep.entries, 7);
            let (es, _) = read_cdx_all(&dir, 1);
            assert_eq!(es.len(), 7);
            let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
            for (e, input) in &all {
                let rec = c.snapshot().read(e, &mut ctx, &mut buf).unwrap();
                if let Some(i) = input {
                    assert_eq!(rec.as_bytes(), &i[..]);
                }
            }
            assert_eq!(es[0].warc_type, "warcinfo");
        }
    }

    #[test]
    fn resume_from_report_offset() {
        use masstuffy_cdx::{CdxReader, Step};
        use std::io::{BufReader, Seek, SeekFrom};
        let (_d, dir) = collection_with_singles(false, 2);
        let (mut c, rep) = open_ok(&dir);
        let mut p = c.preparer().unwrap();
        let es = commit_batch(&mut c, &mut p, (10..13).map(|i| record(i, 40)).collect());
        let mut f = fs::File::open(c.cdx_path()).unwrap();
        f.seek(SeekFrom::Start(rep.cdx_len)).unwrap();
        let mut r = CdxReader::resume(BufReader::new(f), 1, rep.cdx_len, DEFAULT_MAX_LINE).unwrap();
        for want in &es {
            match r.next_step().unwrap() {
                Step::Entry { entry, .. } => assert_eq!(&entry.into_owned(), want),
                Step::End(_) => panic!("short"),
            }
        }
        assert!(matches!(
            r.next_step().unwrap(),
            Step::End(Tail::Clean { .. })
        ));
    }

    #[test]
    fn recovered_concurrent_readers() {
        use std::sync::RwLock;
        use std::sync::atomic::{AtomicBool, Ordering};
        let b = Base::new(true, false, DEFAULT_SPLIT_THRESHOLD);
        let case = b.fresh();
        truncate(&case.join("index.1.cdx"), b.cdx_b - 3);
        let (c, rep) = open_ok(&case);
        assert!(rep.cdx_torn.is_some());
        let recovered = usize::try_from(rep.entries).unwrap();
        let (mut writer, snap) = c.into_parts();
        let published: RwLock<Vec<(Entry<'static>, Option<Vec<u8>>)>> = RwLock::new(
            b.expected[..recovered]
                .iter()
                .cloned()
                .zip(b.inputs.iter().cloned())
                .collect(),
        );
        let done = AtomicBool::new(false);
        let mut committed = 0u64;
        std::thread::scope(|s| {
            let w = s.spawn(|| {
                let mut p = crate::prepare::Preparer::new(Arc::clone(writer.context())).unwrap();
                let mut next = 1000;
                let mut n_total = 0u64;
                for bi in 0..40 {
                    let n = 1 + bi % 5;
                    let mut batch = Vec::new();
                    let mut want = Vec::new();
                    for k in 0..n {
                        let r = record(next + k, 50 + (next + k) * 37 % 3000);
                        want.push(Some(r.as_bytes().to_vec()));
                        batch.push(p.prepare(r).unwrap());
                    }
                    next += n;
                    n_total += u64::try_from(n).unwrap();
                    let es = writer.commit(&batch).unwrap();
                    published.write().unwrap().extend(es.into_iter().zip(want));
                }
                done.store(true, Ordering::Release);
                n_total
            });
            for t in 0..4usize {
                let (snap, published, done) = (&snap, &published, &done);
                s.spawn(move || {
                    let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
                    let mut x = t + 1;
                    let mut check = |i: usize| {
                        let (e, want) = published.read().unwrap()[i].clone();
                        let rec = snap.read(&e, &mut ctx, &mut buf).unwrap();
                        if let Some(w) = want {
                            assert_eq!(rec.as_bytes(), &w[..]);
                        }
                    };
                    loop {
                        let finished = done.load(Ordering::Acquire);
                        let n = published.read().unwrap().len();
                        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                        check((x >> 8) % n);
                        if finished {
                            (0..n).for_each(&mut check);
                            break;
                        }
                        std::thread::yield_now();
                    }
                });
            }
            committed = w.join().unwrap();
        });
        drop((writer, snap));
        let (_c, rep) = open_ok(&case);
        assert!(rep.is_clean(), "{rep:?}");
        assert_eq!(rep.entries, u64::try_from(recovered).unwrap() + committed);
    }

    #[test]
    fn report_unknown_cap() {
        let d = TestDir::new();
        let (c, _) = create_none(&d.sub());
        drop(c);
        for i in 0..40 {
            fs::write(d.sub().join(format!("junk-{i}")), b"x").unwrap();
        }
        let (_c, rep) = open_ok(&d.sub());
        assert_eq!(rep.unknown.len(), MAX_REPORTED_UNKNOWN);
        assert_eq!(rep.unknown_count, 40);
    }

    #[test]
    fn send_sync() {
        fn ss<T: Send + Sync>() {}
        fn sc<T: Send + Sync + Copy>() {}
        ss::<RecoveryReport>();
        sc::<RecoveryOptions>();
    }

    // ---- Checkpoint.

    fn ck_path(dir: &Path) -> PathBuf {
        dir.join("scan.1.json")
    }

    fn read_ck(dir: &Path) -> CheckpointV1 {
        serde_json::from_slice(&fs::read(ck_path(dir)).unwrap()).unwrap()
    }

    /// 5 single-record commits and a checkpoint at the end.
    fn with_checkpoint() -> (TestDir, PathBuf, Vec<(u64, u64, Entry<'static>)>) {
        let (d, dir) = collection_with_singles(false, 5);
        let (mut c, rep) = open_ok(&dir);
        assert_eq!(rep.checkpoint_written, Some(rep.cdx_len));
        assert!(!c.checkpoint().unwrap());
        drop(c);
        let lines = cdx_lines(&dir, 1);
        (d, dir, lines)
    }

    #[test]
    fn checkpoint_after_commit_then_reopen() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = create_none(&dir);
        let mut p = c.preparer().unwrap();
        for i in 1..=3 {
            commit_batch(&mut c, &mut p, vec![record(i, 100)]);
        }
        assert!(c.checkpoint().unwrap());
        assert!(!c.checkpoint().unwrap());
        let ck = read_ck(&dir);
        assert_eq!(ck.cdx_offset, file_len(&dir.join("index.1.cdx")));
        assert_eq!((ck.entries, ck.version, ck.generation), (4, 1, 1));
        assert_eq!(ck.seg_ends.len(), 1);
        commit_batch(&mut c, &mut p, vec![record(4, 100)]);
        drop((p, c));
        // Checkpoint is at the old offset; the commit after it is scanned.
        let (c, rep) = open_ok(&dir);
        assert_eq!(
            rep.scan,
            ScanMode::Partial {
                from: ck.cdx_offset
            }
        );
        assert_eq!((rep.entries, rep.checkpoint_written), (5, None));
        drop(c);
        let (_c, rep) = open_ok(&dir);
        assert_eq!(
            rep.scan,
            ScanMode::Partial {
                from: ck.cdx_offset
            }
        );
        assert_eq!(rep.entries, 5);
    }

    #[test]
    fn partial_scan_skips_prefix() {
        let (d, dir, lines) = with_checkpoint();
        flip_byte(&dir.join("index.1.cdx"), lines[1].0 + 10);
        let (_c, rep) = open_ok(&dir);
        assert!(matches!(rep.scan, ScanMode::Partial { .. }));
        let p2 = d.path().join("full");
        clone_dir(&dir, &p2);
        match open_with(&p2, &refuse_full()) {
            Err(Error::CdxCorrupt { at, .. }) => assert_eq!(at, lines[1].0),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn fallback_reasons() {
        let (d, dir, lines) = with_checkpoint();
        let cdx_len = file_len(&dir.join("index.1.cdx"));
        let ck = read_ck(&dir);
        let variant = |n: &str| {
            let p = d.path().join(n);
            clone_dir(&dir, &p);
            p
        };
        let reason = |p: &Path| {
            let (_c, rep) = open_ok(p);
            let ScanMode::Full { reason } = rep.scan.clone() else {
                panic!("not full: {rep:?}")
            };
            // A fresh valid checkpoint exists afterwards.
            assert!(checkpoint::load(p, 1).is_ok());
            (reason, rep)
        };
        let write = |p: &Path, off: u64, seg_ends: &[u64]| {
            fs::write(ck_path(p), checkpoint::encode(1, off, ck.entries, seg_ends)).unwrap();
        };

        // CDX shorter than the checkpoint.
        let p = variant("short");
        truncate(&p.join("index.1.cdx"), lines[3].1);
        let (r, rep) = reason(&p);
        assert_eq!(
            r,
            FullScanReason::BeyondCdx {
                offset: cdx_len,
                cdx_len: lines[3].1
            }
        );
        assert_eq!(rep.checkpoint_written, Some(lines[3].1));
        assert_eq!(read_ck(&p).cdx_offset, lines[3].1);

        // Not line aligned.
        let p = variant("align");
        let mid = lines[3].0 + 5;
        write(&p, mid, &ck.seg_ends);
        assert_eq!(reason(&p).0, FullScanReason::NotLineAligned { offset: mid });
        let p = variant("align2");
        write(&p, 3, &ck.seg_ends);
        assert_eq!(reason(&p).0, FullScanReason::NotLineAligned { offset: 3 });

        // Segments shrank or are missing.
        let p = variant("shrunk");
        write(&p, ck.cdx_offset, &[1 << 40]);
        assert!(matches!(
            reason(&p).0,
            FullScanReason::SegmentShrunk { segment: 1, .. }
        ));
        let p = variant("missing");
        write(&p, ck.cdx_offset, &[0, 0]);
        assert_eq!(reason(&p).0, FullScanReason::SegmentMissing { segment: 2 });

        // Generation mismatch; another generation's checkpoint is a leftover.
        let p = variant("gen");
        let txt = fs::read_to_string(ck_path(&p)).unwrap();
        fs::write(
            ck_path(&p),
            txt.replace("\"generation\":1", "\"generation\":2"),
        )
        .unwrap();
        fs::write(p.join("scan.2.json"), &txt).unwrap();
        let (r, rep) = reason(&p);
        assert_eq!(r, FullScanReason::Generation { found: 2 });
        assert!(rep.removed.iter().any(|x| x.ends_with("scan.2.json")));
        assert!(!p.join("scan.2.json").exists());

        // Torn or bad content.
        let good = fs::read(ck_path(&dir)).unwrap();
        let bad: Vec<Vec<u8>> = vec![
            good[..good.len() / 2].to_vec(),
            Vec::new(),
            vec![b' '; 2 << 20],
            String::from_utf8(good.clone())
                .unwrap()
                .replace("\"version\":1", "\"version\":1,\"extra\":0")
                .into_bytes(),
            String::from_utf8(good.clone())
                .unwrap()
                .replace("\"version\":1", "\"version\":2")
                .into_bytes(),
        ];
        for (i, content) in bad.iter().enumerate() {
            let p = variant(&format!("bad{i}"));
            fs::write(ck_path(&p), content).unwrap();
            assert_eq!(reason(&p).0, FullScanReason::Malformed, "{i}");
        }
    }

    #[test]
    fn forced_full_scan() {
        let (_d, dir, _) = with_checkpoint();
        let (_c, rep) = open_with(&dir, &refuse_full()).unwrap();
        assert_eq!(
            rep.scan,
            ScanMode::Full {
                reason: FullScanReason::Forced
            }
        );
        assert_eq!(rep.checkpoint_written, Some(rep.cdx_len));
        assert_eq!(rep.entries, 6);
    }

    #[test]
    fn small_growth_no_rewrite_big_growth_rewrite() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = create_none(&dir);
        assert!(c.checkpoint().unwrap());
        let mut p = c.preparer().unwrap();
        for i in 1..=3 {
            commit_batch(&mut c, &mut p, vec![record(i, 20)]);
        }
        drop((p, c));
        let before = (
            fs::read(ck_path(&dir)).unwrap(),
            fs::metadata(ck_path(&dir)).unwrap().modified().unwrap(),
        );
        let (mut c, rep) = open_ok(&dir);
        assert_eq!(rep.checkpoint_written, None);
        let after = (
            fs::read(ck_path(&dir)).unwrap(),
            fs::metadata(ck_path(&dir)).unwrap().modified().unwrap(),
        );
        assert_eq!(before, after);
        let from = file_len(&dir.join("index.1.cdx"));
        let mut p = c.preparer().unwrap();
        for b in 0..20 {
            let recs = (0..500).map(|k| record(100 + b * 500 + k, 10)).collect();
            commit_batch(&mut c, &mut p, recs);
        }
        let grown = file_len(&dir.join("index.1.cdx")) - from;
        assert!(grown >= CHECKPOINT_MIN_GROWTH, "{grown}");
        drop((p, c));
        let (_c, rep) = open_ok(&dir);
        assert_eq!(rep.checkpoint_written, Some(rep.cdx_len));
        assert_eq!(read_ck(&dir).cdx_offset, rep.cdx_len);
        assert_eq!(read_ck(&dir).entries, 10_004);
    }

    #[test]
    fn repair_removes_stale_checkpoint() {
        let (_d, dir, lines) = with_checkpoint();
        let old = read_ck(&dir).cdx_offset;
        flip_byte(&dir.join("index.1.cdx"), lines[1].0 + 10);
        let o = RecoveryOptions {
            full_scan: true,
            ..repair_opts()
        };
        let (mut c, rep) = open_with(&dir, &o).unwrap();
        assert_eq!(rep.repair.as_ref().map(|r| r.at), Some(lines[1].0));
        assert_eq!(rep.checkpoint_written, None);
        assert!(!ck_path(&dir).exists());
        assert!(!c.checkpoint().unwrap());
        let mut p = c.preparer().unwrap();
        let mut es = Vec::new();
        for i in 0..6 {
            es.extend(commit_batch(&mut c, &mut p, vec![record(200 + i, 100)]));
        }
        assert!(file_len(&dir.join("index.1.cdx")) > old);
        drop((p, c));
        let (c, rep) = open_ok(&dir);
        assert_eq!(
            rep.scan,
            ScanMode::Full {
                reason: FullScanReason::NoCheckpoint
            }
        );
        assert!(rep.is_clean());
        assert_eq!(rep.entries, 7);
        read_all(&c, &es);
    }

    #[test]
    fn partial_open_syncs_unsynced_lines_before_checkpoint() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = create_none(&dir);
        let mut p = c.preparer().unwrap();
        commit_batch(&mut c, &mut p, vec![record(1, 50)]);
        assert!(c.checkpoint().unwrap());
        c.writer().fail = Some(FailPoint::BeforeCdxSync);
        let b = vec![p.prepare(record(2, 50)).unwrap()];
        assert!(c.writer().commit(&b).is_err());
        drop((p, c));
        CDX_SYNCS.with(|c| c.set(0));
        let (mut c, rep) = open_ok(&dir);
        assert!(matches!(rep.scan, ScanMode::Partial { .. }));
        assert_eq!((rep.entries, rep.checkpoint_written), (3, None));
        assert_eq!(CDX_SYNCS.with(std::cell::Cell::get), 1);
        assert!(c.checkpoint().unwrap());
        assert_eq!(read_ck(&dir).cdx_offset, rep.cdx_len);
    }

    #[test]
    fn checkpoint_action_table() {
        use CkAction::{Keep, Remove, Write};
        // (cut, full, grew, overflow, blocked)
        let t = |a, b, c, d, e| checkpoint_action(a, b, c, d, e);
        assert_eq!(t(false, false, false, false, false), Keep);
        assert_eq!(t(false, false, true, false, false), Write);
        assert_eq!(t(false, true, false, false, false), Write);
        assert_eq!(t(true, false, true, false, false), Remove);
        assert_eq!(t(false, true, true, true, false), Remove);
        // Partial scan with an unrepresentable segment: never write, keep the old one.
        assert_eq!(t(false, false, true, true, false), Keep);
        assert_eq!(t(false, true, true, false, true), Keep);
    }

    #[test]
    fn checkpoint_too_large_is_not_written() {
        let d = TestDir::new();
        let big = vec![u64::MAX; MAX_CHECKPOINT_SEGMENTS];
        assert!(!checkpoint::write(d.path(), 1, 1, 1, &big).unwrap());
        assert!(!ck_path(d.path()).exists());
    }

    #[test]
    fn directory_named_like_checkpoint_is_unknown() {
        let (_d, dir) = collection_with_singles(false, 2);
        fs::create_dir(ck_path(&dir)).unwrap();
        let (mut c, rep) = open_ok(&dir);
        assert_eq!(rep.unknown, [ck_path(&dir)]);
        assert_eq!(
            rep.scan,
            ScanMode::Full {
                reason: FullScanReason::NotAFile
            }
        );
        assert_eq!(rep.checkpoint_written, None);
        assert!(!c.checkpoint().unwrap());
        assert!(ck_path(&dir).is_dir());
    }

    #[test]
    fn crash_between_repair_truncate_and_unlink() {
        let (d, dir, lines) = with_checkpoint();
        let stale = fs::read(ck_path(&dir)).unwrap();
        flip_byte(&dir.join("index.1.cdx"), lines[2].0 + 10);
        let o = RecoveryOptions {
            full_scan: true,
            ..repair_opts()
        };
        let (c, rep) = open_with(&dir, &o).unwrap();
        drop(c);
        assert_eq!(rep.repair.as_ref().map(|r| r.at), Some(lines[2].0));
        // The unlink did not happen.
        fs::write(ck_path(&dir), stale).unwrap();
        let (_c, rep) = open_ok(&dir);
        assert!(matches!(
            rep.scan,
            ScanMode::Full {
                reason: FullScanReason::BeyondCdx { .. }
            }
        ));
        assert_eq!(rep.entries, 2);
        drop(d);
    }

    #[test]
    fn repair_cut_during_partial_scan() {
        let (_d, dir, _) = with_checkpoint();
        let from = read_ck(&dir).cdx_offset;
        let (mut c, _) = open_ok(&dir);
        let mut p = c.preparer().unwrap();
        for i in 0..3 {
            commit_batch(&mut c, &mut p, vec![record(300 + i, 60)]);
        }
        drop((p, c));
        let lines = cdx_lines(&dir, 1);
        let bad = &lines[7];
        assert!(bad.0 > from);
        flip_byte(&dir.join("index.1.cdx"), bad.0 + 10);
        let (_c, rep) = open_with(&dir, &repair_opts()).unwrap();
        assert_eq!(rep.scan, ScanMode::Partial { from });
        assert_eq!(rep.repair.as_ref().map(|r| r.at), Some(bad.0));
        assert_eq!(rep.entries, 7);
        assert!(!ck_path(&dir).exists());
    }
}
