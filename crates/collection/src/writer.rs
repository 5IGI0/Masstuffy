//! The single appender of one generation: data append, data fsync, CDX append, CDX fsync.

use std::borrow::Cow;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use masstuffy_cdx::{CdxWriter, Entry, Kind};
use masstuffy_seglog::Appender;

use crate::checkpoint::{self, MAX_CHECKPOINT_SEGMENTS};
use crate::context::GenContext;
use crate::error::{CommitError, Error};
use crate::prepare::Prepared;

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
    fn record(&mut self, bytes: usize, lines: usize, segment: u32, end: Option<u64>) {
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
            let e = Entry {
                kind: Kind::Add,
                segment: loc.segment,
                offset: loc.offset,
                raw_size: loc.len,
                digest: p.digest,
                date: p.meta.date,
                warc_type: Cow::Owned(p.meta.warc_type.clone()),
                record_id: Cow::Owned(p.meta.record_id.clone()),
                url: p.meta.url.clone().map(Cow::Owned),
            };
            if let Err(err) = self.cdx.push(&e) {
                // Unreachable thanks to the probe in `prepare`; the durable data are orphans (I3).
                self.cdx.discard_pending();
                return Err(CommitError::Entry(err));
            }
            entries.push(e);
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
        if let (Some(first), Some(last)) = (locs.first(), locs.last()) {
            // All items share one segment (I9).
            self.scan
                .record(written, entries.len(), first.segment, last.end());
        } else {
            self.scan.valid = false;
        }
        Ok(entries)
    }
}
