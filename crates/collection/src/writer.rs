//! The single appender of one generation: data append, data fsync, CDX append, CDX fsync.

use std::borrow::Cow;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use masstuffy_cdx::{CdxWriter, Entry, Kind};
use masstuffy_seglog::Appender;

use crate::context::GenContext;
use crate::error::CommitError;
use crate::prepare::Prepared;

/// Test-only fault injection.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum FailPoint {
    BeforeCdxWrite,
    BeforeCdxSync,
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
    #[cfg(test)]
    pub(crate) fail: Option<FailPoint>,
}

impl Writer {
    pub(crate) fn from_parts(
        ctx: Arc<GenContext>,
        appender: Appender,
        cdx: CdxWriter<File>,
        cdx_path: PathBuf,
    ) -> Writer {
        Writer {
            ctx,
            appender,
            cdx,
            cdx_path,
            poisoned: false,
            #[cfg(test)]
            fail: None,
        }
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
        Ok(entries)
    }
}
