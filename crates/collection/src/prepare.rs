//! Preparation of records: serialise/compress (or validate a delegated raw record) and compute
//! the index fields, outside any lock.

use std::borrow::Cow;
use std::sync::Arc;

use masstuffy_cdx::{DEFAULT_MAX_LINE, Entry, ImportMeta, Kind, digest, encode_entry};
use masstuffy_warc::{Record, RecordRef, WarcDate};
use masstuffy_zframe::{
    CodecError, Compressor, DecoderDict, Decompressor, EncoderDict, inspect_frame,
    inspect_single_frame,
};

use crate::PLAIN_FRAME_MIN;
use crate::context::GenContext;
use crate::error::PrepareError;
use crate::manifest::Codec;

/// A scratch buffer larger than this is released after use.
const SCRATCH_KEEP: usize = 4 << 20;

/// How deeply [`Preparer::prepare_raw`] checks a delegated record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RawCheck {
    /// Decode the payload (decompress if Zstd), parse the WARC record, compare it with the
    /// envelope metadata; index fields come from the record. Default.
    #[default]
    Full,
    /// Cheap checks only, no decompression and no comparison: index fields come from the envelope
    /// metadata (validated by the CDX probe). The caller trusts the client's content.
    ///
    /// A record whose envelope disagrees with its payload, or whose Zstd payload is not a WARC
    /// record, is then readable only with `Snapshot::read_raw`: `Snapshot::read` fails with
    /// `EntryMismatch` or `Record`.
    HeaderOnly,
}

/// One record ready to commit: its stored bytes (WARC or one zstd frame) and its index fields.
///
/// Invariants: `bytes` is non-empty and at most `max_stored_bytes`; `digest` is the digest of
/// `bytes`; the entry built from it at any locator encodes within `DEFAULT_MAX_LINE`.
#[derive(Debug)]
pub struct Prepared {
    pub(crate) ctx: Arc<GenContext>,
    pub(crate) bytes: Vec<u8>,
    pub(crate) digest: u64,
    pub(crate) meta: Meta,
}

impl Prepared {
    /// Size of the stored bytes, for queue byte budgets.
    #[must_use]
    pub fn stored_len(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    /// The context this record was prepared for.
    #[must_use]
    pub fn context(&self) -> &Arc<GenContext> {
        &self.ctx
    }

    /// `WARC-Record-ID` without `<>`.
    #[must_use]
    pub fn record_id(&self) -> &str {
        &self.meta.record_id
    }
}

impl AsRef<[u8]> for Prepared {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// The index fields of a record that do not depend on its locator.
#[derive(Debug)]
pub(crate) struct Meta {
    pub(crate) warc_type: String,
    pub(crate) record_id: String,
    pub(crate) date: WarcDate,
    /// `None` if absent or empty.
    pub(crate) url: Option<String>,
}

fn meta_of(view: &RecordRef<'_>) -> Result<Meta, PrepareError> {
    Ok(Meta {
        warc_type: view.warc_type().to_owned(),
        record_id: view
            .record_id()
            .ok_or(PrepareError::MissingHeader("WARC-Record-ID"))?
            .to_owned(),
        date: view
            .date()
            .ok_or(PrepareError::MissingHeader("WARC-Date"))?,
        url: view
            .target_uri()
            .filter(|u| !u.is_empty())
            .map(str::to_owned),
    })
}

fn meta_of_import(m: &ImportMeta<'_>) -> Meta {
    Meta {
        warc_type: m.warc_type.to_string(),
        record_id: m.record_id.to_string(),
        date: m.date,
        url: m
            .url
            .as_deref()
            .filter(|u| !u.is_empty())
            .map(str::to_owned),
    }
}

/// Index fields of an existing entry (already validated when it was first prepared).
fn meta_of_entry(e: &Entry<'_>) -> Meta {
    Meta {
        warc_type: e.warc_type.to_string(),
        record_id: e.record_id.to_string(),
        date: e.date,
        url: e.url.as_deref().map(str::to_owned),
    }
}

/// Compresses `src` for a Zstd collection: no dictionary above `PLAIN_FRAME_MIN`.
fn encode(z: &mut ZstdState, src: &[u8]) -> Result<Vec<u8>, PrepareError> {
    let mut out = Vec::new();
    if u64::try_from(src.len()).map_or(true, |n| n > PLAIN_FRAME_MIN) {
        z.compressor.compress_plain(src, z.level, &mut out)?;
    } else {
        z.compressor.compress(src, &z.encoder, &mut out)?;
    }
    // `compress` reserves the compress bound; a queued record must not keep the slack.
    out.shrink_to_fit();
    Ok(out)
}

/// Second-precision envelope dates (legacy) match a record date with nanoseconds.
fn same_date(claim: &WarcDate, rec: &WarcDate) -> bool {
    claim.year() == rec.year()
        && claim.month() == rec.month()
        && claim.day() == rec.day()
        && claim.hour() == rec.hour()
        && claim.minute() == rec.minute()
        && claim.second() == rec.second()
        && (claim.nanos() == 0 || claim.nanos() == rec.nanos())
}

fn compare_meta(claim: &ImportMeta<'_>, rec: &Meta) -> Result<(), PrepareError> {
    if *claim.warc_type != *rec.warc_type {
        return Err(PrepareError::MetaMismatch("warc_type"));
    }
    if *claim.record_id != *rec.record_id {
        return Err(PrepareError::MetaMismatch("record_id"));
    }
    let claim_url = claim.url.as_deref().filter(|u| !u.is_empty());
    if claim_url != rec.url.as_deref() {
        return Err(PrepareError::MetaMismatch("url"));
    }
    if !same_date(&claim.date, &rec.date) {
        return Err(PrepareError::MetaMismatch("date"));
    }
    Ok(())
}

/// Checks that an entry built from `m` at the widest possible locator encodes within the line cap,
/// so that `CdxWriter::push` cannot fail at commit.
fn probe(line: &mut Vec<u8>, m: &Meta) -> Result<(), PrepareError> {
    let e = Entry {
        kind: Kind::Add,
        segment: u32::MAX,
        offset: 10_000_000_000_000_000_000,
        raw_size: 1_000_000_000_000_000_000,
        digest: 0,
        date: m.date,
        warc_type: Cow::Borrowed(&m.warc_type),
        record_id: Cow::Borrowed(&m.record_id),
        url: m.url.as_deref().map(Cow::Borrowed),
    };
    line.clear();
    let r = encode_entry(&e, DEFAULT_MAX_LINE, line);
    line.clear();
    r.map_err(PrepareError::from)
}

/// Compression state of a Zstd collection.
struct ZstdState {
    compressor: Compressor,
    decompressor: Option<Decompressor>,
    encoder: Arc<EncoderDict>,
    decoder: Arc<DecoderDict>,
    dict_id: u32,
    level: i32,
}

/// Per-worker preparation state: one compression context, lazily one decompression context, and
/// reusable scratch buffers. `Send`, not shared: a caller running prepares on N threads creates N
/// `Preparer`s. The dictionaries are shared through the `Arc<GenContext>`.
pub struct Preparer {
    ctx: Arc<GenContext>,
    zstd: Option<ZstdState>,
    /// Decompressed raw payload (`prepare_raw`, Zstd, `Full`).
    scratch: Vec<u8>,
    /// CDX probe line.
    line: Vec<u8>,
    /// Decompression context of `reprepare`, created on first use.
    redecode: Option<Decompressor>,
}

impl Preparer {
    /// Creates a preparer for `ctx`.
    ///
    /// # Errors
    ///
    /// `Codec(ContextAlloc)` if zstd cannot allocate a context.
    pub fn new(ctx: Arc<GenContext>) -> Result<Preparer, PrepareError> {
        let zstd = match (ctx.codec, &ctx.encoder, &ctx.decoder) {
            (Codec::None, _, _) => None,
            (Codec::Zstd { dict_id, level }, Some(enc), Some(dec)) => Some(ZstdState {
                compressor: Compressor::new()?,
                decompressor: None,
                encoder: Arc::clone(enc),
                decoder: Arc::clone(dec),
                dict_id: dict_id.get(),
                level,
            }),
            // GenContext::new always sets both for Zstd; kept as an error, not a panic.
            (Codec::Zstd { .. }, _, _) => return Err(CodecError::InvalidDictionary.into()),
        };
        Ok(Preparer {
            ctx,
            zstd,
            scratch: Vec::new(),
            line: Vec::new(),
            redecode: None,
        })
    }

    /// The context records are prepared for.
    #[must_use]
    pub fn context(&self) -> &Arc<GenContext> {
        &self.ctx
    }

    /// Serialises (None) or compresses (Zstd) `record` and computes its index fields.
    ///
    /// In a Zstd collection a record larger than [`PLAIN_FRAME_MIN`](crate::PLAIN_FRAME_MIN) is
    /// compressed without dictionary. The record must carry `WARC-Record-ID` and `WARC-Date`
    /// (filling them is the engine's policy).
    ///
    /// # Errors
    ///
    /// `RecordTooLarge`, `Record` (does not parse within the collection's limits),
    /// `MissingHeader`, `Entry` (index line would be invalid or too long), `Codec`.
    pub fn prepare(&mut self, record: Record) -> Result<Prepared, PrepareError> {
        let ctx = &self.ctx;
        let len = u64::try_from(record.as_bytes().len()).unwrap_or(u64::MAX);
        if len > ctx.max_record_bytes {
            return Err(PrepareError::RecordTooLarge {
                size: len,
                cap: ctx.max_record_bytes,
            });
        }
        let meta = meta_of(&RecordRef::parse(record.as_bytes(), &ctx.warc_limits)?)?;
        probe(&mut self.line, &meta)?;
        let bytes = match &mut self.zstd {
            None => record.into_bytes(),
            Some(z) => encode(z, record.as_bytes())?,
        };
        Ok(Prepared {
            ctx: Arc::clone(&self.ctx),
            digest: digest(&bytes),
            bytes,
            meta,
        })
    }

    /// Rebuild, raw copy: `stored` are the verified stored bytes of `e` (same codec family and
    /// dictionary); the digest is `e.digest`. Only the index line is probed.
    ///
    /// # Errors
    ///
    /// `Entry` (the line would be too long).
    pub(crate) fn copy_stored(
        &mut self,
        stored: Vec<u8>,
        e: &Entry<'_>,
    ) -> Result<Prepared, PrepareError> {
        let meta = meta_of_entry(e);
        probe(&mut self.line, &meta)?;
        Ok(Prepared {
            ctx: Arc::clone(&self.ctx),
            bytes: stored,
            digest: e.digest,
            meta,
        })
    }

    /// Rebuild, transcode: `plain` is the decoded record of `e`; index fields are those of `e`.
    ///
    /// # Errors
    ///
    /// `RecordTooLarge`, `Record` (target None and `plain` is not one WARC record), `Entry`,
    /// `Codec`.
    pub(crate) fn transcode(
        &mut self,
        plain: Vec<u8>,
        e: &Entry<'_>,
    ) -> Result<Prepared, PrepareError> {
        self.encode_plain(plain, meta_of_entry(e))
    }

    /// Size check, probe and encoding of the decoded record `plain` for this generation.
    fn encode_plain(&mut self, plain: Vec<u8>, meta: Meta) -> Result<Prepared, PrepareError> {
        let len = u64::try_from(plain.len()).unwrap_or(u64::MAX);
        if len > self.ctx.max_record_bytes {
            return Err(PrepareError::RecordTooLarge {
                size: len,
                cap: self.ctx.max_record_bytes,
            });
        }
        probe(&mut self.line, &meta)?;
        let bytes = match &mut self.zstd {
            None => {
                RecordRef::parse(&plain, &self.ctx.warc_limits)?;
                plain
            }
            Some(z) => encode(z, &plain)?,
        };
        Ok(Prepared {
            ctx: Arc::clone(&self.ctx),
            digest: digest(&bytes),
            bytes,
            meta,
        })
    }

    /// Re-targets `p` (prepared for an earlier generation of the same collection) to this
    /// preparer's generation: same index fields, stored bytes decoded with `p`'s codec and
    /// re-encoded with this one (records over `PLAIN_FRAME_MIN` without dictionary, as `prepare`).
    /// Returns `p` unchanged if it already has this preparer's collection and generation.
    /// Target codec None requires the decoded bytes to be one WARC record (`Record` otherwise:
    /// a `HeaderOnly` raw import whose payload is not a WARC cannot move to a None generation,
    /// the same rule as the rebuild).
    ///
    /// # Errors
    ///
    /// `ForeignContext` (other collection), `Frame`, `Codec` (decoding), `RecordTooLarge`,
    /// `Record`, `Entry`, `Codec` (encoding).
    pub fn reprepare(&mut self, p: Prepared) -> Result<Prepared, PrepareError> {
        if p.ctx.collection != self.ctx.collection {
            return Err(PrepareError::ForeignContext);
        }
        if p.ctx.generation == self.ctx.generation {
            return Ok(p);
        }
        let Prepared {
            ctx, bytes, meta, ..
        } = p;
        let plain = match ctx.codec {
            Codec::None => bytes,
            Codec::Zstd { .. } => {
                let info = inspect_frame(&bytes)?;
                let dict = if info.dict_id == 0 {
                    None
                } else {
                    Some(
                        ctx.decoder
                            .as_deref()
                            .ok_or(CodecError::InvalidDictionary)?,
                    )
                };
                let d = match &mut self.redecode {
                    Some(d) => d,
                    slot @ None => slot.insert(Decompressor::new()?),
                };
                let mut out = Vec::new();
                d.decompress(&bytes, dict, ctx.max_record_bytes, &mut out)?;
                out
            }
        };
        self.encode_plain(plain, meta)
    }

    /// Validates a delegated raw record (spec 04 4.2b, 06 "Validation de l'import brut") and
    /// computes its index fields. `payload` is moved into the result.
    ///
    /// At both levels: non-empty, length equal to `meta.raw_size`, within the stored-size cap,
    /// digest equal to `meta.digest` if claimed, and a complete WARC record (None) or exactly one
    /// zstd frame obeying the dictionary rule (Zstd: the collection's dictionary id, or no
    /// dictionary with a declared content size above `PLAIN_FRAME_MIN`) and declaring no more than
    /// `max_record_bytes`. With [`RawCheck::Full`] the frame is also decompressed and parsed and the
    /// envelope metadata must match the record; with [`RawCheck::HeaderOnly`] the index fields
    /// come from `meta`.
    ///
    /// # Errors
    ///
    /// `EmptyPayload`, `SizeMismatch`, `StoredTooLarge`, `DigestMismatch`, `RecordTooLarge`,
    /// `Record`, `Frame`, `UnexpectedDictId`, `PlainFrameNotAllowed`, `DeclaredTooLarge`, `Codec`,
    /// `MetaMismatch`, `Entry`.
    pub fn prepare_raw(
        &mut self,
        meta: &ImportMeta<'_>,
        payload: Vec<u8>,
        check: RawCheck,
    ) -> Result<Prepared, PrepareError> {
        if payload.is_empty() {
            return Err(PrepareError::EmptyPayload);
        }
        let len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        if len != meta.raw_size {
            return Err(PrepareError::SizeMismatch {
                declared: meta.raw_size,
                actual: len,
            });
        }
        let ctx = &self.ctx;
        if len > ctx.max_stored_bytes {
            return Err(PrepareError::StoredTooLarge {
                size: len,
                cap: ctx.max_stored_bytes,
            });
        }
        let actual = digest(&payload);
        if let Some(claimed) = meta.digest
            && claimed != actual
        {
            return Err(PrepareError::DigestMismatch { claimed, actual });
        }
        let index = match &mut self.zstd {
            None => {
                if len > ctx.max_record_bytes {
                    return Err(PrepareError::RecordTooLarge {
                        size: len,
                        cap: ctx.max_record_bytes,
                    });
                }
                let view = RecordRef::parse(&payload, &ctx.warc_limits)?;
                match check {
                    RawCheck::Full => {
                        let m = meta_of(&view)?;
                        compare_meta(meta, &m)?;
                        m
                    }
                    RawCheck::HeaderOnly => meta_of_import(meta),
                }
            }
            Some(z) => {
                let info = inspect_single_frame(&payload)?;
                let use_dict = if info.dict_id == z.dict_id {
                    true
                } else if info.dict_id == 0 {
                    match info.content_size {
                        Some(n) if n > PLAIN_FRAME_MIN => false,
                        content_size => {
                            return Err(PrepareError::PlainFrameNotAllowed { content_size });
                        }
                    }
                } else {
                    return Err(PrepareError::UnexpectedDictId {
                        frame: info.dict_id,
                        expected: z.dict_id,
                    });
                };
                let cap = ctx.max_record_bytes;
                let declared = info.content_size.unwrap_or(info.window_size);
                if declared > cap {
                    return Err(PrepareError::DeclaredTooLarge {
                        size: declared,
                        cap,
                    });
                }
                match check {
                    RawCheck::HeaderOnly => meta_of_import(meta),
                    RawCheck::Full => {
                        let r = decode_full(z, &mut self.scratch, &payload, use_dict, ctx, meta);
                        if self.scratch.capacity() > SCRATCH_KEEP {
                            self.scratch = Vec::new();
                        }
                        r?
                    }
                }
            }
        };
        probe(&mut self.line, &index)?;
        Ok(Prepared {
            ctx: Arc::clone(&self.ctx),
            bytes: payload,
            digest: actual,
            meta: index,
        })
    }
}

/// Decompresses `payload` into `scratch`, parses it and compares it with the envelope.
fn decode_full(
    z: &mut ZstdState,
    scratch: &mut Vec<u8>,
    payload: &[u8],
    use_dict: bool,
    ctx: &GenContext,
    claim: &ImportMeta<'_>,
) -> Result<Meta, PrepareError> {
    let d = match &mut z.decompressor {
        Some(d) => d,
        slot @ None => slot.insert(Decompressor::new()?),
    };
    let dict = use_dict.then_some(&*z.decoder);
    d.decompress(payload, dict, ctx.max_record_bytes, scratch)?;
    let view = RecordRef::parse(scratch, &ctx.warc_limits)?;
    let m = meta_of(&view)?;
    compare_meta(claim, &m)?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Options;
    use crate::manifest::Manifest;
    use crate::testutil::{
        MapResolver, id_str, manifest_none, manifest_zstd, other_dict, other_id, record,
        record_of_size, trained_id,
    };
    use masstuffy_warc::ParseError;
    use masstuffy_zframe::{FrameError, inspect_frame};

    fn ctx_with(zstd: bool, opts: &Options) -> Arc<GenContext> {
        let m = if zstd {
            manifest_zstd()
        } else {
            manifest_none()
        };
        Arc::new(GenContext::new(&m, opts, &MapResolver::standard()).unwrap())
    }

    fn preparer(zstd: bool) -> Preparer {
        Preparer::new(ctx_with(zstd, &Options::default())).unwrap()
    }

    fn small(max: u64) -> Options {
        Options {
            max_record_bytes: max,
            ..Options::default()
        }
    }

    /// Envelope metadata describing `rec`, with the given stored bytes.
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

    fn frame_with(dict: &[u8], src: &[u8]) -> Vec<u8> {
        let enc = EncoderDict::new(dict, 1).unwrap();
        let mut out = Vec::new();
        Compressor::new()
            .unwrap()
            .compress(src, &enc, &mut out)
            .unwrap();
        out
    }

    fn plain_frame(src: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Compressor::new()
            .unwrap()
            .compress_plain(src, 1, &mut out)
            .unwrap();
        out
    }

    fn dict_frame(src: &[u8]) -> Vec<u8> {
        frame_with(crate::testutil::base_dict(), src)
    }

    /// A preparer for generation `generation` of collection `n` with `codec`.
    fn gen_preparer(n: u128, generation: u32, codec: Codec) -> Preparer {
        let m = Manifest::new(
            crate::testutil::uuid(n),
            "test",
            crate::DEFAULT_SPLIT_THRESHOLD,
            crate::manifest::Generation {
                id: generation,
                codec,
            },
            generation + 1,
        )
        .unwrap();
        let ctx = GenContext::new(&m, &Options::default(), &MapResolver::standard()).unwrap();
        Preparer::new(Arc::new(ctx)).unwrap()
    }

    /// Decodes the stored bytes of `p` back to the WARC record bytes.
    fn plain_of(p: &Prepared) -> Vec<u8> {
        match p.ctx.codec {
            Codec::None => p.bytes.clone(),
            Codec::Zstd { .. } => {
                let info = inspect_frame(&p.bytes).unwrap();
                let dict = (info.dict_id != 0).then(|| p.ctx.decoder.clone().unwrap());
                let mut out = Vec::new();
                Decompressor::new()
                    .unwrap()
                    .decompress(&p.bytes, dict.as_deref(), u64::MAX, &mut out)
                    .unwrap();
                out
            }
        }
    }

    #[test]
    fn reprepare_round_trips() {
        let none = Codec::None;
        let d1 = crate::testutil::zstd_codec();
        let d2 = crate::testutil::other_codec();
        for (from, to) in [(none, d1), (d1, d2), (d1, none), (none, none), (d1, d1)] {
            let mut src = gen_preparer(0, 1, from);
            let mut dst = gen_preparer(0, 2, to);
            let r = record(7, 400);
            let want = r.as_bytes().to_vec();
            let p = src.prepare(r).unwrap();
            let q = dst.reprepare(p).unwrap();
            assert_eq!(q.ctx.generation, 2);
            assert_eq!(plain_of(&q), want);
            assert_eq!(q.digest, digest(&q.bytes));
            assert_eq!(q.record_id(), id_str(7));
            if let Codec::Zstd { dict_id, .. } = to {
                assert_eq!(inspect_frame(&q.bytes).unwrap().dict_id, dict_id.get());
            }
        }
    }

    #[test]
    fn reprepare_same_generation_unchanged() {
        let mut src = gen_preparer(0, 1, crate::testutil::zstd_codec());
        let p = src.prepare(record(1, 100)).unwrap();
        let (bytes, d) = (p.bytes.clone(), p.digest);
        let q = src.reprepare(p).unwrap();
        assert_eq!((q.bytes, q.digest), (bytes, d));
    }

    #[test]
    fn reprepare_foreign_collection() {
        let mut a = gen_preparer(0, 1, Codec::None);
        let mut b = gen_preparer(1, 2, Codec::None);
        let p = a.prepare(record(1, 100)).unwrap();
        assert!(matches!(b.reprepare(p), Err(PrepareError::ForeignContext)));
    }

    #[test]
    fn reprepare_header_only_non_warc() {
        let d1 = crate::testutil::zstd_codec();
        let d2 = crate::testutil::other_codec();
        let mut src = gen_preparer(0, 1, d1);
        let rec = record(3, 50);
        let payload = b"not a warc record at all, just bytes".to_vec();
        let stored = dict_frame(&payload);
        let meta = meta_for(&rec, &stored);
        let p = src
            .prepare_raw(&meta, stored, RawCheck::HeaderOnly)
            .unwrap();
        let mut to_zstd = gen_preparer(0, 2, d2);
        let q = to_zstd.reprepare(p).unwrap();
        assert_eq!(plain_of(&q), payload);
        assert_eq!(q.record_id(), id_str(3));

        let stored = dict_frame(&payload);
        let p = src
            .prepare_raw(&meta_for(&rec, &stored), stored, RawCheck::HeaderOnly)
            .unwrap();
        let mut to_none = gen_preparer(0, 3, Codec::None);
        assert!(matches!(to_none.reprepare(p), Err(PrepareError::Record(_))));
    }

    #[test]
    fn reprepare_large_record_is_plain_frame() {
        let mut src = gen_preparer(0, 1, Codec::None);
        let mut dst = gen_preparer(0, 2, crate::testutil::zstd_codec());
        let r = record_of_size(1, 3 << 19);
        let want = r.as_bytes().to_vec();
        let q = dst.reprepare(src.prepare(r).unwrap()).unwrap();
        assert_eq!(inspect_frame(&q.bytes).unwrap().dict_id, 0);
        assert_eq!(plain_of(&q), want);
    }

    #[test]
    fn prepare_fields() {
        let mut p = preparer(true);
        let r = record(5, 100);
        let bytes = r.as_bytes().to_vec();
        let pr = p.prepare(r).unwrap();
        assert_eq!(pr.meta.warc_type, "response");
        assert_eq!(pr.record_id(), id_str(5));
        assert_eq!(pr.meta.url.as_deref(), Some("https://example.org/5"));
        assert_eq!(pr.meta.date.to_string(), "2026-11-13T10:00:00Z");
        assert_eq!(pr.digest, digest(pr.as_ref()));
        assert_eq!(pr.stored_len(), u64::try_from(pr.as_ref().len()).unwrap());
        assert_ne!(pr.as_ref(), &bytes[..]);

        let mk = |extra: &str| {
            let raw = format!(
                "WARC/1.1\r\nWARC-Type: resource\r\nWARC-Record-ID: <urn:uuid:1>\r\nWARC-Date: 2026-11-13T10:00:00Z\r\n{extra}Content-Length: 0\r\n\r\n\r\n\r\n"
            );
            Record::from(RecordRef::parse(raw.as_bytes(), &Default::default()).unwrap())
        };
        assert_eq!(p.prepare(mk("")).unwrap().meta.url, None);
        assert_eq!(
            p.prepare(mk("WARC-Target-URI: \r\n")).unwrap().meta.url,
            None
        );
    }

    #[test]
    fn prepare_none_no_copy() {
        let mut p = preparer(false);
        let r = record(1, 50);
        let bytes = r.as_bytes().to_vec();
        let pr = p.prepare(r).unwrap();
        assert_eq!(pr.as_ref(), &bytes[..]);
        assert_eq!(pr.digest, digest(&bytes));
    }

    #[test]
    fn prepare_zstd_dict_frame() {
        let mut p = preparer(true);
        let r = record(2, 200);
        let n = u64::try_from(r.as_bytes().len()).unwrap();
        let pr = p.prepare(r).unwrap();
        let info = inspect_frame(pr.as_ref()).unwrap();
        assert_eq!(info.dict_id, trained_id());
        assert_eq!(info.content_size, Some(n));
    }

    #[test]
    fn prepare_zstd_large_plain() {
        let mut p = preparer(true);
        let min = usize::try_from(PLAIN_FRAME_MIN).unwrap();
        let big = p.prepare(record_of_size(1, min + 1)).unwrap();
        assert_eq!(inspect_frame(big.as_ref()).unwrap().dict_id, 0);
        let edge = p.prepare(record_of_size(2, min)).unwrap();
        assert_eq!(inspect_frame(edge.as_ref()).unwrap().dict_id, trained_id());
    }

    #[test]
    fn prepare_missing_id_or_date() {
        let mut p = preparer(false);
        let no_id = "WARC/1.1\r\nWARC-Type: resource\r\nWARC-Date: 2026-11-13T10:00:00Z\r\nContent-Length: 0\r\n\r\n\r\n\r\n";
        let no_date = "WARC/1.1\r\nWARC-Type: resource\r\nWARC-Record-ID: <urn:uuid:1>\r\nContent-Length: 0\r\n\r\n\r\n\r\n";
        let rec =
            |s: &str| Record::from(RecordRef::parse(s.as_bytes(), &Default::default()).unwrap());
        assert!(matches!(
            p.prepare(rec(no_id)),
            Err(PrepareError::MissingHeader("WARC-Record-ID"))
        ));
        assert!(matches!(
            p.prepare(rec(no_date)),
            Err(PrepareError::MissingHeader("WARC-Date"))
        ));
    }

    #[test]
    fn prepare_oversized() {
        let mut p = Preparer::new(ctx_with(false, &small(1024))).unwrap();
        assert!(matches!(
            p.prepare(record(1, 2048)),
            Err(PrepareError::RecordTooLarge { .. })
        ));
        let mut p = preparer(false);
        let mut b = masstuffy_warc::RecordBuilder::new("resource").unwrap();
        b.record_id("urn:uuid:1").unwrap();
        b.date(WarcDate::parse("2026-11-13T10:00:00Z").unwrap());
        b.header("X-Big", &"a".repeat(70 * 1024)).unwrap();
        let r = b.build(b"").unwrap();
        assert!(matches!(
            p.prepare(r),
            Err(PrepareError::Record(
                ParseError::HeaderSectionTooLarge { .. }
            ))
        ));
    }

    fn zstd_rejects(meta: &ImportMeta<'_>, payload: Vec<u8>, check: RawCheck) -> PrepareError {
        preparer(true)
            .prepare_raw(meta, payload, check)
            .unwrap_err()
    }

    #[test]
    fn prepare_raw_rejects() {
        let rec = record(1, 300);
        let good = dict_frame(rec.as_bytes());
        let m = meta_for(&rec, &good);
        let mut p = preparer(true);

        let mut e = m.clone();
        e.raw_size = 0;
        assert!(matches!(
            p.prepare_raw(&e, Vec::new(), RawCheck::Full),
            Err(PrepareError::EmptyPayload)
        ));
        let mut e = m.clone();
        e.raw_size += 1;
        assert!(matches!(
            p.prepare_raw(&e, good.clone(), RawCheck::Full),
            Err(PrepareError::SizeMismatch { .. })
        ));
        let mut e = m.clone();
        e.digest = Some(digest(&good) ^ 1);
        assert!(matches!(
            p.prepare_raw(&e, good.clone(), RawCheck::Full),
            Err(PrepareError::DigestMismatch { .. })
        ));
        let junk = vec![7u8; 64];
        assert!(matches!(
            zstd_rejects(&meta_for(&rec, &junk), junk, RawCheck::Full),
            PrepareError::Frame(FrameError::BadMagic(_))
        ));
        let two = [good.clone(), good.clone()].concat();
        assert!(matches!(
            zstd_rejects(&meta_for(&rec, &two), two, RawCheck::Full),
            PrepareError::Frame(FrameError::TrailingBytes { .. })
        ));
        let other = frame_with(&other_dict(), rec.as_bytes());
        assert!(matches!(
            zstd_rejects(&meta_for(&rec, &other), other, RawCheck::Full),
            PrepareError::UnexpectedDictId { expected, frame }
                if expected == trained_id() && frame == other_id()
        ));
        let plain = plain_frame(rec.as_bytes());
        assert!(matches!(
            zstd_rejects(&meta_for(&rec, &plain), plain, RawCheck::Full),
            PrepareError::PlainFrameNotAllowed { .. }
        ));
        let nonwarc = dict_frame(b"definitely not a warc record");
        assert!(matches!(
            zstd_rejects(&meta_for(&rec, &nonwarc), nonwarc, RawCheck::Full),
            PrepareError::Record(_)
        ));
        let two_recs = dict_frame(&[rec.as_bytes(), rec.as_bytes()].concat());
        assert!(matches!(
            zstd_rejects(&meta_for(&rec, &two_recs), two_recs, RawCheck::Full),
            PrepareError::Record(ParseError::TrailingData { .. })
        ));
        // Envelope disagrees with the record.
        for field in ["record_id", "warc_type", "url", "date"] {
            let mut e = m.clone();
            match field {
                "record_id" => e.record_id = Cow::Borrowed("urn:uuid:other"),
                "warc_type" => e.warc_type = Cow::Borrowed("resource"),
                "url" => e.url = None,
                _ => e.date = WarcDate::parse("2026-11-13T10:00:01Z").unwrap(),
            }
            let r = p.prepare_raw(&e, good.clone(), RawCheck::Full);
            assert!(
                matches!(r, Err(PrepareError::MetaMismatch(f)) if f == field),
                "{field}: {r:?}"
            );
        }
        // Stored-size cap.
        let mut p = Preparer::new(ctx_with(true, &small(1024))).unwrap();
        let big = vec![1u8; 140_000];
        assert!(matches!(
            p.prepare_raw(&meta_for(&rec, &big), big, RawCheck::Full),
            Err(PrepareError::StoredTooLarge { .. })
        ));
    }

    #[test]
    fn raw_full_declared_too_large() {
        let mut p = Preparer::new(ctx_with(true, &small(1024))).unwrap();
        let rec = record(1, 2048);
        let f = dict_frame(rec.as_bytes());
        let r = p.prepare_raw(&meta_for(&rec, &f), f, RawCheck::Full);
        assert!(
            matches!(r, Err(PrepareError::DeclaredTooLarge { .. })),
            "{r:?}"
        );
    }

    #[test]
    fn prepare_raw_accepts() {
        let mut p = preparer(true);
        // A dict frame made by another preparer.
        let rec = record(3, 500);
        let pr = preparer(true).prepare(record(3, 500)).unwrap();
        let stored = pr.as_ref().to_vec();
        let out = p
            .prepare_raw(&meta_for(&rec, &stored), stored.clone(), RawCheck::Full)
            .unwrap();
        assert_eq!(out.as_ref(), &stored[..]);
        assert_eq!(out.digest, pr.digest);
        // A plain frame of a > 1 MiB record.
        let min = usize::try_from(PLAIN_FRAME_MIN).unwrap();
        let big = record_of_size(4, min + 1);
        let f = plain_frame(big.as_bytes());
        assert!(
            p.prepare_raw(&meta_for(&big, &f), f, RawCheck::Full)
                .is_ok()
        );
        // Second precision envelope date for a record with nanoseconds; no claimed digest.
        let mut b = masstuffy_warc::RecordBuilder::new("resource").unwrap();
        b.record_id("urn:uuid:9").unwrap();
        b.date(WarcDate::parse("2026-11-13T10:00:00.123456Z").unwrap());
        let nanos = b.build(b"x").unwrap();
        let f = dict_frame(nanos.as_bytes());
        let mut m = meta_for(&nanos, &f);
        m.date = WarcDate::parse("2026-11-13T10:00:00Z").unwrap();
        m.digest = None;
        let out = p.prepare_raw(&m, f, RawCheck::Full).unwrap();
        assert_eq!(out.meta.date.nanos(), 123_456_000);
    }

    #[test]
    fn prepare_raw_none() {
        let mut p = preparer(false);
        let rec = record(1, 100);
        let ok = rec.as_bytes().to_vec();
        let out = p
            .prepare_raw(&meta_for(&rec, &ok), ok.clone(), RawCheck::Full)
            .unwrap();
        assert_eq!(out.as_ref(), &ok[..]);
        let frame = dict_frame(&ok);
        let garbage = [ok.clone(), b"zz".to_vec()].concat();
        let two = [ok.clone(), ok.clone()].concat();
        for bad in [frame, garbage, two] {
            let r = p.prepare_raw(&meta_for(&rec, &bad), bad, RawCheck::Full);
            assert!(matches!(r, Err(PrepareError::Record(_))), "{r:?}");
        }
    }

    #[test]
    fn raw_full_empty_claimed_url_is_none() {
        let mut p = preparer(false);
        let mut b = masstuffy_warc::RecordBuilder::new("resource").unwrap();
        b.record_id("urn:uuid:7").unwrap();
        b.date(WarcDate::parse("2026-11-13T10:00:00Z").unwrap());
        let rec = b.build(b"x").unwrap();
        let mut m = meta_for(&rec, rec.as_bytes());
        m.url = Some(Cow::Borrowed(""));
        let out = p
            .prepare_raw(&m, rec.as_bytes().to_vec(), RawCheck::Full)
            .unwrap();
        assert_eq!(out.meta.url, None);
    }

    #[test]
    fn raw_check_default_is_full() {
        assert_eq!(RawCheck::default(), RawCheck::Full);
    }

    #[test]
    fn raw_header_only_accepts_mismatch() {
        for zstd in [true, false] {
            let mut p = preparer(zstd);
            let rec = record(1, 100);
            let stored = if zstd {
                dict_frame(rec.as_bytes())
            } else {
                rec.as_bytes().to_vec()
            };
            let mut m = meta_for(&rec, &stored);
            m.record_id = Cow::Borrowed("urn:uuid:other");
            m.warc_type = Cow::Borrowed("resource");
            m.url = Some(Cow::Borrowed("https://other.example/"));
            m.date = WarcDate::parse("2020-01-02T03:04:05Z").unwrap();
            let r = p.prepare_raw(&m, stored.clone(), RawCheck::Full);
            assert!(matches!(r, Err(PrepareError::MetaMismatch(_))), "{r:?}");
            let out = p
                .prepare_raw(&m, stored.clone(), RawCheck::HeaderOnly)
                .unwrap();
            assert_eq!(out.meta.record_id, "urn:uuid:other");
            assert_eq!(out.meta.warc_type, "resource");
            assert_eq!(out.meta.url.as_deref(), Some("https://other.example/"));
            assert_eq!(out.meta.date, m.date);
            assert_eq!(out.as_ref(), &stored[..]);
        }
    }

    #[test]
    fn raw_header_only_skips_decode() {
        let mut p = preparer(true);
        let rec = record(1, 100);
        let f = dict_frame(b"not a warc record");
        let m = meta_for(&rec, &f);
        assert!(matches!(
            p.prepare_raw(&m, f.clone(), RawCheck::Full),
            Err(PrepareError::Record(_))
        ));
        assert!(p.prepare_raw(&m, f, RawCheck::HeaderOnly).is_ok());
    }

    #[test]
    fn raw_header_only_still_checks() {
        let h = RawCheck::HeaderOnly;
        let rec = record(1, 300);
        let good = dict_frame(rec.as_bytes());
        let m = meta_for(&rec, &good);
        let mut p = preparer(true);

        let mut e = m.clone();
        e.raw_size = 0;
        assert!(matches!(
            p.prepare_raw(&e, Vec::new(), h),
            Err(PrepareError::EmptyPayload)
        ));
        let mut e = m.clone();
        e.raw_size += 1;
        assert!(matches!(
            p.prepare_raw(&e, good.clone(), h),
            Err(PrepareError::SizeMismatch { .. })
        ));
        let mut e = m.clone();
        e.digest = Some(digest(&good) ^ 1);
        assert!(matches!(
            p.prepare_raw(&e, good.clone(), h),
            Err(PrepareError::DigestMismatch { .. })
        ));
        let junk = vec![7u8; 64];
        assert!(matches!(
            zstd_rejects(&meta_for(&rec, &junk), junk, h),
            PrepareError::Frame(FrameError::BadMagic(_))
        ));
        let two = [good.clone(), good.clone()].concat();
        assert!(matches!(
            zstd_rejects(&meta_for(&rec, &two), two, h),
            PrepareError::Frame(FrameError::TrailingBytes { .. })
        ));
        let other = frame_with(&other_dict(), rec.as_bytes());
        assert!(matches!(
            zstd_rejects(&meta_for(&rec, &other), other, h),
            PrepareError::UnexpectedDictId { .. }
        ));
        let plain = plain_frame(rec.as_bytes());
        assert!(matches!(
            zstd_rejects(&meta_for(&rec, &plain), plain, h),
            PrepareError::PlainFrameNotAllowed { .. }
        ));
        let mut small_p = Preparer::new(ctx_with(true, &small(1024))).unwrap();
        let bigrec = record(2, 2048);
        let f = dict_frame(bigrec.as_bytes());
        assert!(matches!(
            small_p.prepare_raw(&meta_for(&bigrec, &f), f, h),
            Err(PrepareError::DeclaredTooLarge { .. })
        ));
        let mut e = m.clone();
        e.record_id = Cow::Borrowed("has space");
        assert!(matches!(
            p.prepare_raw(&e, good, h),
            Err(PrepareError::Entry(_))
        ));
    }

    #[test]
    fn raw_header_only_none_still_parses() {
        let mut p = preparer(false);
        let rec = record(1, 100);
        let bad = [rec.as_bytes(), b"trailing"].concat();
        let r = p.prepare_raw(&meta_for(&rec, &bad), bad, RawCheck::HeaderOnly);
        assert!(matches!(r, Err(PrepareError::Record(_))), "{r:?}");
    }

    #[test]
    fn scratch_released() {
        let mut p = Preparer::new(ctx_with(true, &Options::default())).unwrap();
        let big = record_of_size(1, 5 << 20);
        let f = plain_frame(big.as_bytes());
        p.prepare_raw(&meta_for(&big, &f), f, RawCheck::Full)
            .unwrap();
        assert_eq!(p.scratch.capacity(), 0);
    }
}
