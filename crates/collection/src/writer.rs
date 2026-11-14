//! The single appender of one generation: data append, data fsync, CDX append, CDX fsync.

use std::borrow::Cow;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use masstuffy_cdx::{CdxWriter, Entry, Kind};
use masstuffy_seglog::{Appender, Locator};

use crate::checkpoint::{self, MAX_CHECKPOINT_SEGMENTS};
use crate::context::GenContext;
use crate::error::{CommitError, Error};
use crate::prepare::Prepared;
use crate::tombstone::PreparedTombstone;

/// Test-only fault injection.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum FailPoint {
    BeforeCdxWrite,
    BeforeCdxSync,
}

/// What a checkpoint needs, for the CDX bytes fsynced so far. Owned by the [`Writer`].
#[derive(Debug)]
pub(crate) struct ScanState {
    /// Length of the fsynced CDX (a line boundary).
    pub(crate) cdx_len: u64,
    /// Number of lines in `[header, cdx_len)`.
    pub(crate) entries: u64,
    /// Per segment `i + 1`: maximum end of the lines pointing to it.
    pub(crate) seg_ends: Vec<u64>,
    /// False once the state cannot describe the file (overflow, repair cut, too many segments).
    pub(crate) valid: bool,
}

impl ScanState {
    /// Accounts for a batch whose CDX bytes were fsynced. O(1), no I/O.
    pub(crate) fn record(&mut self, bytes: usize, lines: usize, segment: u32, end: Option<u64>) {
        let cdx_len = u64::try_from(bytes)
            .ok()
            .and_then(|b| self.cdx_len.checked_add(b));
        let entries = u64::try_from(lines)
            .ok()
            .and_then(|l| self.entries.checked_add(l));
        let seg = usize::try_from(segment)
            .ok()
            .filter(|s| (1..=MAX_CHECKPOINT_SEGMENTS).contains(s));
        let (Some(cdx_len), Some(entries), Some(seg), Some(end)) = (cdx_len, entries, seg, end)
        else {
            self.valid = false;
            return;
        };
        if self.seg_ends.len() < seg {
            self.seg_ends.resize(seg, 0);
        }
        if let Some(e) = self.seg_ends.get_mut(seg - 1) {
            *e = (*e).max(end);
        }
        self.cdx_len = cdx_len;
        self.entries = entries;
    }

    /// Accounts for `bytes` bytes and `lines` lines of fsynced CDX. Returns false (and invalidates
    /// the state) on overflow.
    pub(crate) fn add_lines(&mut self, bytes: usize, lines: usize) -> bool {
        let cdx_len = u64::try_from(bytes)
            .ok()
            .and_then(|b| self.cdx_len.checked_add(b));
        let entries = u64::try_from(lines)
            .ok()
            .and_then(|l| self.entries.checked_add(l));
        if let (Some(cdx_len), Some(entries)) = (cdx_len, entries) {
            self.cdx_len = cdx_len;
            self.entries = entries;
            true
        } else {
            self.valid = false;
            false
        }
    }

    /// Raises the end of `segment` to `end`; invalidates the state if the segment number is out of
    /// the checkpoint's range.
    pub(crate) fn bump(&mut self, segment: u32, end: u64) {
        let seg = usize::try_from(segment)
            .ok()
            .filter(|s| (1..=MAX_CHECKPOINT_SEGMENTS).contains(s));
        let Some(seg) = seg else {
            self.valid = false;
            return;
        };
        if self.seg_ends.len() < seg {
            self.seg_ends.resize(seg, 0);
        }
        if let Some(e) = self.seg_ends.get_mut(seg - 1) {
            *e = (*e).max(end);
        }
    }
}

/// The index entry of `p` committed at `loc` (borrows `p`).
pub(crate) fn entry_for<'p>(p: &'p Prepared, loc: &Locator) -> Entry<'p> {
    Entry {
        kind: Kind::Add,
        segment: loc.segment,
        offset: loc.offset,
        raw_size: loc.len,
        digest: p.digest,
        date: p.meta.date,
        warc_type: Cow::Borrowed(&p.meta.warc_type),
        record_id: Cow::Borrowed(&p.meta.record_id),
        url: p.meta.url.as_deref().map(Cow::Borrowed),
    }
}

/// The single appender of one generation of one collection. `&mut self` only: the caller
/// guarantees one `Writer` per collection (the engine's writer task). Not meant to be shared.
pub struct Writer {
    ctx: Arc<GenContext>,
    appender: Appender,
    /// Opened in append mode, positioned after the last complete line.
    cdx: CdxWriter<File>,
    /// For error messages.
    cdx_path: PathBuf,
    poisoned: bool,
    /// Length of the CDX covered by a successful fsync (a line boundary).
    synced_len: u64,
    /// Lines fsynced by this writer and by the recovered prefix.
    scan: ScanState,
    /// CDX offset of the last checkpoint known to be on disk (0 if none).
    pub(crate) last_checkpoint: u64,
    #[cfg(test)]
    pub(crate) fail: Option<FailPoint>,
}

impl Writer {
    pub(crate) fn from_parts(
        ctx: Arc<GenContext>,
        appender: Appender,
        cdx: CdxWriter<File>,
        cdx_path: PathBuf,
        scan: ScanState,
    ) -> Writer {
        Writer {
            ctx,
            appender,
            cdx,
            cdx_path,
            poisoned: false,
            synced_len: scan.cdx_len,
            scan,
            last_checkpoint: 0,
            #[cfg(test)]
            fail: None,
        }
    }

    /// Path of the generation's CDX, for re-reading it with `CdxReader::resume` (secondary-index
    /// catch-up).
    #[must_use]
    pub fn cdx_path(&self) -> &Path {
        &self.cdx_path
    }

    /// Deletes the files of every generation other than this writer's (`seg.<g>.*`,
    /// `index.<g>.cdx`, `scan.<g>.json`, `g != generation()`), then fsyncs the directory if
    /// anything was removed. Returns the paths removed. Unknown files are kept; `.tmp-*` are not
    /// touched (a checkpoint write may be in flight).
    ///
    /// Precondition: no `Rebuilder` of this collection is alive (the engine calls it from the
    /// writer task after `Rebuilder::finish` failed without committing). Idempotent.
    ///
    /// # Errors
    ///
    /// `Io`, `Durable`.
    pub fn remove_stale_generations(&self) -> Result<Vec<PathBuf>, Error> {
        let dir = self
            .cdx_path
            .parent()
            .ok_or(Error::Internal("cdx path has no parent"))?;
        let active = crate::manifest::Generation {
            id: self.ctx.generation(),
            codec: self.ctx.codec(),
        };
        crate::rebuild::remove_leftovers(dir, active)
    }

    /// Length of the CDX made durable by successful commits and tombstones (and the recovered
    /// prefix): a line boundary. Pass it to `Snapshot::entries` and `Rebuilder::copy_until`; the
    /// engine publishes it after each commit for the rebuild task.
    #[must_use]
    pub fn cdx_len(&self) -> u64 {
        self.synced_len
    }

    /// After a generation switch the old writer must never commit again.
    pub(crate) fn retire(&mut self) {
        self.poisoned = true;
    }

    /// Adds `written` fsynced CDX bytes to `synced_len`.
    fn advance_synced(&mut self, written: usize) -> Result<(), CommitError> {
        let next = u64::try_from(written)
            .ok()
            .and_then(|w| self.synced_len.checked_add(w));
        match next {
            Some(n) => {
                self.synced_len = n;
                Ok(())
            }
            None => {
                self.poisoned = true;
                Err(CommitError::CdxSync {
                    path: self.cdx_path.clone(),
                    source: std::io::Error::other("cdx length overflow"),
                })
            }
        }
    }

    /// Writes `scan.<gen>.json` for every line fsynced by successful commits (and the recovered
    /// prefix), so the next `open` scans only what follows. Never called by `commit`: the owner
    /// calls it between commits (clean shutdown, every N MiB of CDX), in `spawn_blocking`.
    ///
    /// Returns `Ok(false)` without I/O if nothing is new since the last checkpoint, if there are
    /// more than `MAX_CHECKPOINT_SEGMENTS` segments, or if a repair cut made the state invalid
    /// (the next `open` with a full scan restores it).
    ///
    /// # Errors
    ///
    /// `Commit(Poisoned)` on a poisoned writer, `Internal` if the CDX path has no parent,
    /// `Durable` if the atomic write fails.
    pub fn checkpoint(&mut self) -> Result<bool, Error> {
        if self.poisoned {
            return Err(Error::Commit(CommitError::Poisoned));
        }
        if !self.scan.valid
            || self.scan.seg_ends.len() > MAX_CHECKPOINT_SEGMENTS
            || self.scan.cdx_len == self.last_checkpoint
        {
            return Ok(false);
        }
        let dir = self
            .cdx_path
            .parent()
            .ok_or(Error::Internal("cdx path has no parent"))?;
        if !checkpoint::write(
            dir,
            self.ctx.generation(),
            self.scan.cdx_len,
            self.scan.entries,
            &self.scan.seg_ends,
        )? {
            return Ok(false);
        }
        self.last_checkpoint = self.scan.cdx_len;
        Ok(true)
    }

    /// The generation context.
    #[must_use]
    pub fn context(&self) -> &Arc<GenContext> {
        &self.ctx
    }

    /// The generation number being written.
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.ctx.generation()
    }

    /// True after an I/O error during a commit: every later commit fails with `Poisoned`; drop the
    /// writer and reopen the collection.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Makes `batch` durable and indexed; returns one entry per item, in order.
    ///
    /// Order: data append, data fsync, CDX append, CDX fsync. The batch is contiguous in one
    /// segment. No compression happens here: items come from [`Preparer`](crate::Preparer)s.
    ///
    /// On `Poisoned`, `WrongGeneration`, `Entry` and the non-I/O `Data` errors
    /// (`BatchTooLarge`, `SegmentLimit`, empty item) nothing was indexed. After a poisoning error
    /// (`Data` with I/O, `CdxWrite`, `CdxSync`) the batch is unacknowledged: after a crash or
    /// reopen none, a prefix or all of its CDX lines may exist (each points to fsynced data), so
    /// delivery is at-least-once and a duplicate `record_id` is possible.
    ///
    /// # Errors
    ///
    /// See [`CommitError`].
    pub fn commit(&mut self, batch: &[Prepared]) -> Result<Vec<Entry<'static>>, CommitError> {
        if self.poisoned {
            return Err(CommitError::Poisoned);
        }
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        for (index, p) in batch.iter().enumerate() {
            let c = p.ctx.as_ref();
            if c.collection() != self.ctx.collection() || c.generation() != self.ctx.generation() {
                return Err(CommitError::WrongGeneration {
                    index,
                    expected: self.ctx.generation(),
                    got: c.generation(),
                });
            }
        }
        let locs = match self.appender.append_batch(batch) {
            Ok(l) => l,
            Err(e) => {
                if self.appender.is_poisoned() {
                    self.poisoned = true;
                }
                return Err(CommitError::Data(e));
            }
        };
        if let Err(e) = self.appender.sync() {
            self.poisoned = true;
            return Err(CommitError::Data(e));
        }
        // Data is durable. Build the index lines.
        let mut entries = Vec::with_capacity(batch.len());
        for (p, loc) in batch.iter().zip(&locs) {
            let e = entry_for(p, loc);
            if let Err(err) = self.cdx.push(&e) {
                // Unreachable thanks to the probe in `prepare`; the durable data are orphans (I3).
                self.cdx.discard_pending();
                return Err(CommitError::Entry(err));
            }
            entries.push(e.into_owned());
        }
        #[cfg(test)]
        if matches!(self.fail, Some(FailPoint::BeforeCdxWrite)) {
            self.poisoned = true;
            return Err(CommitError::CdxWrite {
                path: self.cdx_path.clone(),
                source: std::io::Error::other("injected").into(),
            });
        }
        let written = self.cdx.pending();
        if let Err(source) = self.cdx.write_pending() {
            self.poisoned = true;
            return Err(CommitError::CdxWrite {
                path: self.cdx_path.clone(),
                source,
            });
        }
        #[cfg(test)]
        if matches!(self.fail, Some(FailPoint::BeforeCdxSync)) {
            self.poisoned = true;
            return Err(CommitError::CdxSync {
                path: self.cdx_path.clone(),
                source: std::io::Error::other("injected"),
            });
        }
        // fdatasync persists the size change; the CDX directory entry was synced at creation.
        if let Err(source) = self.cdx.get_ref().sync_data() {
            self.poisoned = true;
            return Err(CommitError::CdxSync {
                path: self.cdx_path.clone(),
                source,
            });
        }
        self.advance_synced(written)?;
        if let (Some(first), Some(last)) = (locs.first(), locs.last()) {
            // All items share one segment (I9).
            self.scan
                .record(written, entries.len(), first.segment, last.end());
        } else {
            self.scan.valid = false;
        }
        Ok(entries)
    }

    /// A `T` line must not point into bytes that a crash could drop (orphans of an earlier
    /// process, still in the page cache): recovery would then cut the CDX at that line. With a
    /// valid scan state, require `end <= seg_ends[segment]` (committed, fsynced lines only).
    /// Otherwise fall back to the segment's file length and fsync the segment first.
    fn check_committed(&mut self, items: &[PreparedTombstone]) -> Result<(), CommitError> {
        for (index, t) in items.iter().enumerate() {
            let segment = t.entry.segment;
            let end = t.entry.end().unwrap_or(u64::MAX);
            let committed = if self.scan.valid {
                usize::try_from(segment)
                    .ok()
                    .and_then(|s| s.checked_sub(1))
                    .and_then(|i| self.scan.seg_ends.get(i).copied())
                    .unwrap_or(0)
            } else {
                let path = self.appender.dir().join(format!(
                    "{}.{}.{}",
                    self.appender.naming().stem(),
                    segment,
                    self.appender.naming().ext()
                ));
                let sync = |path: &Path| {
                    let f = File::open(path)?;
                    f.sync_data()?;
                    f.metadata().map(|m| m.len())
                };
                match sync(&path) {
                    Ok(len) => len,
                    // A missing segment is a bad target, not an I/O failure: reject, don't poison.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
                    Err(source) => {
                        self.poisoned = true;
                        return Err(CommitError::CdxSync { path, source });
                    }
                }
            };
            if end > committed {
                return Err(CommitError::TombstoneNotCommitted {
                    index,
                    segment,
                    end,
                    committed,
                });
            }
        }
        Ok(())
    }

    /// Makes the tombstones durable: appends one `T` line per item to the CDX, then fsyncs it. No
    /// data is written. Returns the `T` entries, in order (apply them to the secondary index).
    ///
    /// Every target must be covered by a committed line (`TombstoneNotCommitted` otherwise, not
    /// poisoning). Same error contract as `commit`: `Poisoned`, `WrongGeneration` (prepared against another
    /// collection or generation, e.g. before a rebuild switch: prepare again), `Entry`
    /// (unreachable: probed), and the poisoning `CdxWrite`, `CdxSync`. Duplicates are allowed (a
    /// second `T` for the same locator is harmless). After a crash before the fsync the `T` line
    /// may be absent or torn (`open` truncates a torn tail): the tombstone was not acknowledged
    /// and may be retried.
    ///
    /// # Errors
    ///
    /// See [`CommitError`].
    pub fn tombstone(
        &mut self,
        items: &[PreparedTombstone],
    ) -> Result<Vec<Entry<'static>>, CommitError> {
        if self.poisoned {
            return Err(CommitError::Poisoned);
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }
        for (index, t) in items.iter().enumerate() {
            let c = t.ctx.as_ref();
            if c.collection() != self.ctx.collection() || c.generation() != self.ctx.generation() {
                return Err(CommitError::WrongGeneration {
                    index,
                    expected: self.ctx.generation(),
                    got: c.generation(),
                });
            }
        }
        self.check_committed(items)?;
        for t in items {
            if let Err(e) = self.cdx.push(&t.entry) {
                self.cdx.discard_pending();
                return Err(CommitError::Entry(e));
            }
        }
        #[cfg(test)]
        if matches!(self.fail, Some(FailPoint::BeforeCdxWrite)) {
            self.poisoned = true;
            return Err(CommitError::CdxWrite {
                path: self.cdx_path.clone(),
                source: std::io::Error::other("injected").into(),
            });
        }
        let written = self.cdx.pending();
        if let Err(source) = self.cdx.write_pending() {
            self.poisoned = true;
            return Err(CommitError::CdxWrite {
                path: self.cdx_path.clone(),
                source,
            });
        }
        #[cfg(test)]
        if matches!(self.fail, Some(FailPoint::BeforeCdxSync)) {
            self.poisoned = true;
            return Err(CommitError::CdxSync {
                path: self.cdx_path.clone(),
                source: std::io::Error::other("injected"),
            });
        }
        if let Err(source) = self.cdx.get_ref().sync_data() {
            self.poisoned = true;
            return Err(CommitError::CdxSync {
                path: self.cdx_path.clone(),
                source,
            });
        }
        self.advance_synced(written)?;
        if self.scan.add_lines(written, items.len()) {
            for t in items {
                // `prepare_tombstone` checked `end <= segment length` and segments never shrink.
                match t.entry.end() {
                    Some(end) => self.scan.bump(t.entry.segment, end),
                    None => self.scan.valid = false,
                }
            }
        }
        Ok(items.iter().map(|t| t.entry.clone()).collect())
    }
}
