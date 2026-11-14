//! Bulk raw import from an envelope stream (spec 04 4.2b, "Records bruts").

use std::io::{BufReader, Read};
use std::sync::atomic::Ordering;

use masstuffy_cdx::{
    DEFAULT_MAX_LINE, EnvelopeError, EnvelopeFormat, EnvelopeLimits, EnvelopeReader,
};
use masstuffy_collection::{PrepareError, Prepared, Preparer, RawCheck};
use tokio::sync::mpsc;

use crate::collection::CollectionHandle;
use crate::error::Error;
use crate::types::RawItem;

/// Largest value of [`ImportOptions::batch_records`].
pub const MAX_IMPORT_BATCH_RECORDS: usize = 65_536;
/// At most this many [`Rejection`]s are listed in an [`ImportReport`] (the count is exact).
pub const MAX_REPORTED_REJECTIONS: usize = 64;

/// What to do with a record that fails validation (`prepare_raw`). Framing errors (bad line,
/// truncated payload, oversize `raw_size`, I/O) always stop the import: the stream position is
/// lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnInvalid {
    /// Stop at the first invalid record (default). Every record before it is committed.
    #[default]
    Abort,
    /// Skip it, count it, report it (at most [`MAX_REPORTED_REJECTIONS`]), and continue.
    Skip,
}

/// Options of [`CollectionHandle::put_raw_envelope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportOptions {
    /// What to do with an invalid record.
    pub on_invalid: OnInvalid,
    /// Records per batch (one writer request each), `1..=MAX_IMPORT_BATCH_RECORDS`. Default 4096.
    pub batch_records: usize,
    /// Payload bytes per batch; `None` = `min(max_group_bytes, split_threshold)` of the
    /// collection. `Some(n)`: `1 <= n`, and it is clamped to that same maximum. A record larger
    /// than the cap forms a batch alone.
    pub batch_bytes: Option<u64>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            on_invalid: OnInvalid::Abort,
            batch_records: 4096,
            batch_bytes: None,
        }
    }
}

/// One skipped record (`OnInvalid::Skip`).
#[derive(Debug)]
pub struct Rejection {
    /// Stream offset of the record's envelope line.
    pub offset: u64,
    /// 0-based ordinal of the record in the stream.
    pub ordinal: u64,
    /// The claimed `record_id`.
    pub record_id: String,
    /// Why it was rejected.
    pub error: PrepareError,
}

/// Progress of an import, returned on success and inside `Error::Import` on failure.
#[derive(Debug, Default)]
pub struct ImportReport {
    /// Envelope format (`None` for an empty stream or a failure before the first line).
    pub format: Option<EnvelopeFormat>,
    /// Records committed (durable, acknowledged by the writer task).
    pub records: u64,
    /// Writer requests committed.
    pub batches: u64,
    /// Records skipped (`OnInvalid::Skip`); exact count.
    pub rejected: u64,
    /// The first [`MAX_REPORTED_REJECTIONS`] rejections.
    pub rejections: Vec<Rejection>,
    /// Stream offset (header included) such that every record starting before it is either
    /// durable or rejected, and no record starting at or after it was written. Resume by sending
    /// the header line (V1) again, followed by the input bytes from this offset.
    pub committed_offset: u64,
}

enum Feed {
    Format(EnvelopeFormat),
    Batch(RawBatch),
    End { offset: u64 },
    Failed(EnvelopeError),
}

struct RawBatch {
    items: Vec<RawItem>,
    offsets: Vec<u64>,
    first_ordinal: u64,
    bytes: u64,
    end_offset: u64,
}

impl RawBatch {
    fn new(batch_records: usize, first_ordinal: u64) -> RawBatch {
        let cap = batch_records.min(1024);
        RawBatch {
            items: Vec::with_capacity(cap),
            offsets: Vec::with_capacity(cap),
            first_ordinal,
            bytes: 0,
            end_offset: 0,
        }
    }
}

struct Abort {
    offset: u64,
    ordinal: u64,
    error: PrepareError,
}

#[derive(Default)]
struct BatchOutcome {
    rejected: u64,
    rejections: Vec<Rejection>,
    abort: Option<Abort>,
}

fn len_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Reads the envelope on a blocking thread and cuts it into batches. Holds no permit, gate guard
/// or lock: it writes nothing, so it never delays shutdown. Returns as soon as a send fails (the
/// consumer is gone).
fn produce<R: Read>(
    input: R,
    limits: EnvelopeLimits,
    batch_records: usize,
    batch_bytes: u64,
    tx: &mpsc::Sender<Feed>,
) {
    let mut r = match EnvelopeReader::new(BufReader::with_capacity(64 * 1024, input), limits) {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.blocking_send(Feed::Failed(e));
            return;
        }
    };
    if tx.blocking_send(Feed::Format(r.format())).is_err() {
        return;
    }
    let mut ordinal = 0u64;
    let mut batch = RawBatch::new(batch_records, ordinal);
    loop {
        let at = r.offset();
        let step = r.next_item();
        let (meta, payload) = match step {
            Ok(Some(item)) => (item.meta.into_owned(), item.payload),
            Ok(None) => {
                let offset = r.offset();
                if !batch.items.is_empty() && tx.blocking_send(Feed::Batch(batch)).is_err() {
                    return;
                }
                let _ = tx.blocking_send(Feed::End { offset });
                return;
            }
            Err(e) => {
                if !batch.items.is_empty() && tx.blocking_send(Feed::Batch(batch)).is_err() {
                    return;
                }
                let _ = tx.blocking_send(Feed::Failed(e));
                return;
            }
        };
        let len = len_u64(payload.len());
        // Flush before the record that would cross the threshold (spec 4.2b).
        if !batch.items.is_empty() && batch.bytes.saturating_add(len) > batch_bytes {
            let next = RawBatch::new(batch_records, ordinal);
            if tx
                .blocking_send(Feed::Batch(std::mem::replace(&mut batch, next)))
                .is_err()
            {
                return;
            }
        }
        batch.items.push(RawItem { meta, payload });
        batch.offsets.push(at);
        batch.end_offset = r.offset();
        batch.bytes = batch.bytes.saturating_add(len);
        ordinal = ordinal.saturating_add(1);
        if batch.items.len() >= batch_records {
            let next = RawBatch::new(batch_records, ordinal);
            if tx
                .blocking_send(Feed::Batch(std::mem::replace(&mut batch, next)))
                .is_err()
            {
                return;
            }
        }
    }
}

/// The prepare step of one batch (runs in the blocking pool, inside `put_stage`).
fn prepare_import_batch(
    check: RawCheck,
    on_invalid: OnInvalid,
    offsets: Vec<u64>,
    first_ordinal: u64,
) -> impl FnOnce(&mut Preparer, Vec<RawItem>) -> Result<(Vec<Prepared>, BatchOutcome), Error> + Send
{
    move |p, items| {
        let mut out = Vec::with_capacity(items.len());
        let mut outcome = BatchOutcome::default();
        for (i, item) in items.into_iter().enumerate() {
            let RawItem { meta, payload } = item;
            match p.prepare_raw(&meta, payload, check) {
                Ok(prepared) => out.push(prepared),
                Err(error) => {
                    let ordinal = first_ordinal.saturating_add(len_u64(i));
                    let offset = offsets.get(i).copied().unwrap_or(u64::MAX);
                    match on_invalid {
                        OnInvalid::Abort => {
                            outcome.abort = Some(Abort {
                                offset,
                                ordinal,
                                error,
                            });
                            break;
                        }
                        OnInvalid::Skip => {
                            outcome.rejected = outcome.rejected.saturating_add(1);
                            if outcome.rejections.len() < MAX_REPORTED_REJECTIONS {
                                outcome.rejections.push(Rejection {
                                    offset,
                                    ordinal,
                                    record_id: meta.record_id.into_owned(),
                                    error,
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok((out, outcome))
    }
}

fn stopped(report: ImportReport, source: Error) -> Error {
    Error::Import {
        report: Box::new(report),
        source: Box::new(source),
    }
}

impl CollectionHandle {
    /// Imports an envelope stream (spec 04 4.2b): `#masstuffy-cdx/1` (header line, then entry
    /// lines) or the legacy line format, each line followed by `raw_size` stored bytes. The
    /// format is detected from the first line. Every record is validated with the repository's
    /// [`RawCheck`] exactly like [`put_raw`](Self::put_raw).
    ///
    /// `input` is consumed on one blocking thread, on demand: memory stays bounded by about three
    /// batches (one being read, one queued, one being prepared or committed). To import from an
    /// async source, wrap it in `tokio_util::io::SyncIoBridge` (not a dependency of this crate).
    /// A `Read` that blocks forever keeps its thread until it returns, which is the caller's
    /// responsibility; it does not block shutdown.
    ///
    /// Records are cut into batches of at most `batch_records` records and about `batch_bytes`
    /// payload bytes; each batch is one writer request (one group-commit slot), and only one
    /// batch is in flight at a time, so the committed set is always a prefix of the stream
    /// (minus rejected records). With [`OnInvalid::Abort`] (default) the first invalid record
    /// stops the import after the records before it were committed; with [`OnInvalid::Skip`] it
    /// is counted and reported and the import goes on. Framing errors always stop the import.
    ///
    /// On failure, `Error::Import` carries an [`ImportReport`]: resume a V1 envelope by sending
    /// the header line again followed by the input bytes from `committed_offset`.
    ///
    /// # Cancellation
    ///
    /// Dropping the future stops the import: the batch whose request is already queued is
    /// committed anyway, no new batch is admitted, and the reader thread stops at its next send.
    /// The caller does not learn `committed_offset`, so a retry is at-least-once (duplicates are
    /// allowed). The import is not one atomic flow: it is atomic per writer request only.
    ///
    /// # Errors
    ///
    /// `InvalidQuery` for bad options (nothing is read). Otherwise `Error::Import { report,
    /// source }` where `source` is `Envelope` (framing, I/O, size cap), `Prepare { index =
    /// ordinal, .. }` (`OnInvalid::Abort`), `ShuttingDown`, `Commit`, `Reopen`,
    /// `CollectionUnavailable` (that batch may or may not be durable), `Join` or `Internal`.
    pub async fn put_raw_envelope<R>(
        &self,
        input: R,
        opts: ImportOptions,
    ) -> Result<ImportReport, Error>
    where
        R: Read + Send + 'static,
    {
        if opts.batch_records == 0 || opts.batch_records > MAX_IMPORT_BATCH_RECORDS {
            return Err(Error::InvalidQuery("batch_records"));
        }
        if opts.batch_bytes == Some(0) {
            return Err(Error::InvalidQuery("batch_bytes"));
        }
        let mut report = ImportReport::default();
        if self.core.closed.load(Ordering::Acquire) {
            return Err(stopped(report, Error::ShuttingDown));
        }
        let snap = self.shared.published.load();
        let cap = self
            .core
            .config
            .max_group_bytes
            .min(snap.manifest().split_threshold().get());
        let batch_bytes = opts.batch_bytes.map_or(cap, |b| b.min(cap));
        let limits = EnvelopeLimits {
            max_line: DEFAULT_MAX_LINE,
            max_record: snap.context().max_stored_bytes(),
        };
        drop(snap);
        let check = self.core.raw_check;
        let on_invalid = opts.on_invalid;
        let batch_records = opts.batch_records;

        let (tx, mut rx) = mpsc::channel::<Feed>(1);
        let producer = tokio::task::spawn_blocking(move || {
            produce(input, limits, batch_records, batch_bytes, &tx);
        });
        // Dropping `rx` (return or cancellation) makes the producer's next send fail.
        loop {
            match rx.recv().await {
                Some(Feed::Format(f)) => report.format = Some(f),
                Some(Feed::Batch(b)) => {
                    let RawBatch {
                        items,
                        offsets,
                        first_ordinal,
                        bytes,
                        end_offset,
                    } = b;
                    // One batch at a time: batch k+1 is submitted only after batch k's reply.
                    let staged = self
                        .put_stage(
                            items,
                            bytes,
                            prepare_import_batch(check, on_invalid, offsets, first_ordinal),
                        )
                        .await;
                    let (located, outcome) = match staged {
                        Ok(v) => v,
                        Err(e) => return Err(stopped(report, e)),
                    };
                    if !located.is_empty() {
                        report.records = report.records.saturating_add(len_u64(located.len()));
                        report.batches = report.batches.saturating_add(1);
                    }
                    report.rejected = report.rejected.saturating_add(outcome.rejected);
                    for r in outcome.rejections {
                        if report.rejections.len() < MAX_REPORTED_REJECTIONS {
                            report.rejections.push(r);
                        }
                    }
                    if let Some(a) = outcome.abort {
                        report.committed_offset = a.offset;
                        let index = usize::try_from(a.ordinal).unwrap_or(usize::MAX);
                        return Err(stopped(
                            report,
                            Error::Prepare {
                                index,
                                source: a.error,
                            },
                        ));
                    }
                    report.committed_offset = end_offset;
                }
                Some(Feed::End { offset }) => {
                    if offset == 0 {
                        // An empty stream: the reader's default format is not a detection.
                        report.format = None;
                    }
                    report.committed_offset = offset;
                    return Ok(report);
                }
                Some(Feed::Failed(e)) => return Err(stopped(report, Error::Envelope(e))),
                None => {
                    // The producer ended without `End` or `Failed`: it panicked.
                    let e = match producer.await {
                        Err(join) => Error::Join(join),
                        Ok(()) => Error::Internal("import producer ended early"),
                    };
                    return Err(stopped(report, e));
                }
            }
        }
    }
}
