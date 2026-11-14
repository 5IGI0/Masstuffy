//! Immutable read view of one generation: positioned reads, digest check, decode.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use masstuffy_cdx::{Entry, Kind, digest};
use masstuffy_seglog::{Locator, Reader, ReaderOptions};
use masstuffy_warc::RecordRef;
use masstuffy_zframe::{CodecError, Decompressor, inspect_frame};

use crate::context::{GenContext, Options};
use crate::error::{Error, ReadError};
use crate::layout::{cdx_name, segment_naming};
use crate::manifest::{Codec, Manifest};

/// A compressed-bytes buffer larger than this is released after a read.
const RAW_KEEP: usize = 4 << 20;

/// Immutable read view of one generation. `Send + Sync`; shared as `Arc<Snapshot>`. Reads take no
/// lock other than the segment reader's handle-cache lock (never held during I/O).
pub struct Snapshot {
    manifest: Arc<Manifest>,
    ctx: Arc<GenContext>,
    reader: Reader,
}

/// Per-thread read state: a lazily created decompression context and the compressed-bytes buffer.
#[derive(Default)]
pub struct ReadCtx {
    decompressor: Option<Decompressor>,
    raw: Vec<u8>,
}

impl ReadCtx {
    /// A fresh context.
    #[must_use]
    pub fn new() -> ReadCtx {
        ReadCtx::default()
    }
}

/// What [`Snapshot::read_raw`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawEncoding {
    /// The bytes are one WARC record.
    Warc,
    /// The bytes are one zstd frame; `dict_id` 0 means no dictionary.
    Zstd {
        /// Dictionary id from the frame header.
        dict_id: u32,
    },
}

impl Snapshot {
    pub(crate) fn new(
        manifest: Arc<Manifest>,
        ctx: Arc<GenContext>,
        dir: &Path,
        opts: &Options,
    ) -> Result<Snapshot, Error> {
        let reader = Reader::new(
            dir,
            segment_naming(ctx.generation, &ctx.codec)?,
            ReaderOptions {
                max_open_files: opts.max_open_files,
                max_read_len: ctx.max_stored_bytes,
            },
        );
        Ok(Snapshot {
            manifest,
            ctx,
            reader,
        })
    }

    /// The segment reader (recovery).
    pub(crate) fn reader(&self) -> &Reader {
        &self.reader
    }

    /// The manifest this snapshot was built from.
    #[must_use]
    pub fn manifest(&self) -> &Arc<Manifest> {
        &self.manifest
    }

    /// The generation context.
    #[must_use]
    pub fn context(&self) -> &Arc<GenContext> {
        &self.ctx
    }

    /// The generation number.
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.ctx.generation
    }

    /// One positioned read of `entry`'s stored bytes into `buf`, then the digest check.
    fn fetch(&self, entry: &Entry<'_>, buf: &mut Vec<u8>) -> Result<(), ReadError> {
        if entry.kind != Kind::Add {
            return Err(ReadError::Tombstone);
        }
        let loc = Locator {
            segment: entry.segment,
            offset: entry.offset,
            len: entry.raw_size,
        };
        self.reader.read_at(&loc, buf).map_err(|e| match e {
            masstuffy_seglog::Error::SegmentNotFound { segment } => {
                ReadError::SegmentMissing { segment }
            }
            e => ReadError::Segment(e),
        })?;
        let actual = digest(buf);
        if actual != entry.digest {
            return Err(ReadError::DigestMismatch {
                expected: entry.digest,
                actual,
            });
        }
        Ok(())
    }

    /// Reads, verifies and parses the record of `entry` into `out`; the returned view borrows
    /// `out`. `entry` must belong to this snapshot's generation (the caller compares); it is
    /// untrusted, and every check holds for arbitrary input.
    ///
    /// The digest of the stored bytes is always verified. The record is cross-checked with the
    /// entry's `record_id` and `warc_type`.
    ///
    /// # Errors
    ///
    /// `Tombstone`; `SegmentMissing` (possibly a stale snapshot: retry with the current one);
    /// `Segment` (out of bounds, over `max_stored_bytes`, I/O); `DigestMismatch`; `Frame`,
    /// `Codec` (Zstd); `Record`; `EntryMismatch`.
    pub fn read<'b>(
        &self,
        entry: &Entry<'_>,
        ctx: &mut ReadCtx,
        out: &'b mut Vec<u8>,
    ) -> Result<RecordRef<'b>, ReadError> {
        self.read_plain(entry, ctx, out)?;
        let out: &'b [u8] = out;
        let rec = RecordRef::parse(out, &self.ctx.warc_limits)?;
        if rec.record_id() != Some(&*entry.record_id) {
            return Err(ReadError::EntryMismatch("record_id"));
        }
        if rec.warc_type() != &*entry.warc_type {
            return Err(ReadError::EntryMismatch("warc_type"));
        }
        Ok(rec)
    }

    /// Reads, verifies the digest of and decodes the record of `entry` into `out`, without parsing
    /// it as a WARC record (rebuild, dictionary training).
    ///
    /// # Errors
    ///
    /// `Tombstone`, `SegmentMissing`, `Segment`, `DigestMismatch`, `Frame`, `Codec`.
    pub fn read_plain(
        &self,
        entry: &Entry<'_>,
        ctx: &mut ReadCtx,
        out: &mut Vec<u8>,
    ) -> Result<(), ReadError> {
        match self.ctx.codec {
            Codec::None => self.fetch(entry, out),
            Codec::Zstd { .. } => {
                let ReadCtx { decompressor, raw } = ctx;
                let r = self
                    .fetch(entry, raw)
                    .and_then(|()| self.decode(raw, decompressor, out));
                if raw.capacity() > RAW_KEEP {
                    *raw = Vec::new();
                }
                r
            }
        }
    }

    /// Decodes already fetched and verified stored bytes (`read_raw`): codec None returns `stored`
    /// itself (no copy), Zstd decompresses into a new buffer under `max_record_bytes`.
    ///
    /// # Errors
    ///
    /// `Frame`, `Codec`.
    pub fn decode_stored(&self, stored: Vec<u8>, ctx: &mut ReadCtx) -> Result<Vec<u8>, ReadError> {
        match self.ctx.codec {
            Codec::None => Ok(stored),
            Codec::Zstd { .. } => {
                let mut out = Vec::new();
                self.decode(&stored, &mut ctx.decompressor, &mut out)?;
                Ok(out)
            }
        }
    }

    /// Path of this generation's CDX.
    pub(crate) fn cdx_path(&self) -> PathBuf {
        self.reader.dir().join(cdx_name(self.ctx.generation))
    }

    fn decode(
        &self,
        raw: &[u8],
        decompressor: &mut Option<Decompressor>,
        out: &mut Vec<u8>,
    ) -> Result<(), ReadError> {
        let info = inspect_frame(raw)?;
        let dict = if info.dict_id == 0 {
            None
        } else {
            Some(
                self.ctx
                    .decoder
                    .as_deref()
                    .ok_or(CodecError::InvalidDictionary)?,
            )
        };
        let d = match decompressor {
            Some(d) => d,
            slot @ None => slot.insert(Decompressor::new()?),
        };
        d.decompress(raw, dict, self.ctx.max_record_bytes, out)?;
        Ok(())
    }

    /// Reads the stored bytes of `entry` into `out` (no decoding, no WARC parse), after the
    /// digest check. For a Zstd generation the frame's dictionary id is returned so a client can
    /// fetch the dictionary.
    ///
    /// # Errors
    ///
    /// `Tombstone`, `SegmentMissing`, `Segment`, `DigestMismatch`, `Frame`.
    pub fn read_raw(&self, entry: &Entry<'_>, out: &mut Vec<u8>) -> Result<RawEncoding, ReadError> {
        self.fetch(entry, out)?;
        match self.ctx.codec {
            Codec::None => Ok(RawEncoding::Warc),
            Codec::Zstd { .. } => Ok(RawEncoding::Zstd {
                dict_id: inspect_frame(out)?.dict_id,
            }),
        }
    }
}
