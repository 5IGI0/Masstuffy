//! Prepared dictionaries, reusable (de)compression contexts, one record per zstd frame.
//!
//! Everything here is synchronous CPU work: async callers use `spawn_blocking`.
//!
//! Frames written here carry the dictionary id (0 without a dictionary) and the content size, and
//! have **no** content checksum: the stable prepared-dictionary API cannot emit one. Integrity is
//! covered by the index digest of the stored bytes, the strict dictionary-id check and the content
//! size check.

use crate::dict::{DictError, DictHeader};
use crate::frame::{FrameError, FrameInfo, inspect_single_frame};
use zstd::dict::{DecoderDictionary, EncoderDictionary};
use zstd::zstd_safe::{
    CCtx, CParameter, DCtx, InBuffer, OutBuffer, ResetDirective, compress_bound, get_error_name,
    max_c_level, min_c_level,
};

/// First growth step of the output buffer on the unknown-content-size path.
const MIN_GROW: usize = 64 * 1024;

/// Error from the codec layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodecError {
    /// The frame is not exactly one valid zstd frame.
    #[error("invalid frame: {0}")]
    Frame(#[from] FrameError),
    /// The dictionary header is invalid.
    #[error("invalid dictionary header: {0}")]
    Dict(#[from] DictError),
    /// zstd refused to load the dictionary, or its id differs from the header's.
    #[error("zstd rejected the dictionary")]
    InvalidDictionary,
    /// The compression level is outside zstd's range.
    #[error("invalid compression level {0}")]
    InvalidLevel(i32),
    /// The frame's dictionary id differs from the dictionary supplied (0 = none).
    #[error("frame dictionary id {frame} does not match expected {expected}")]
    DictIdMismatch {
        /// Id found in the frame header.
        frame: u32,
        /// Id of the dictionary supplied (0 when none).
        expected: u32,
    },
    /// The frame declares a content size above the cap.
    #[error("declared content size {size} exceeds cap {cap}")]
    ContentTooLarge {
        /// Declared content size.
        size: u64,
        /// Cap given by the caller.
        cap: u64,
    },
    /// The decompressed output exceeded the cap.
    #[error("decompressed output exceeds cap {cap}")]
    OutputTooLarge {
        /// Cap given by the caller.
        cap: u64,
    },
    /// The frame has no content size and its window is larger than the cap. A valid delegated
    /// frame may be rejected this way.
    #[error("window size {window} exceeds cap {cap}")]
    WindowTooLarge {
        /// Window size from the frame header.
        window: u64,
        /// Cap given by the caller.
        cap: u64,
    },
    /// A size does not fit the platform's integer types.
    #[error("size overflow")]
    SizeOverflow,
    /// zstd could not allocate a context.
    #[error("zstd context allocation failed")]
    ContextAlloc,
    /// zstd reported an error.
    #[error("zstd: {0}")]
    Zstd(&'static str),
}

fn zerr(code: usize) -> CodecError {
    CodecError::Zstd(get_error_name(code))
}

fn to_u64(n: usize) -> Result<u64, CodecError> {
    u64::try_from(n).map_err(|_| CodecError::SizeOverflow)
}

/// Checks that `level` is within zstd's supported range (`min_c_level()..=max_c_level()`).
///
/// # Errors
///
/// [`CodecError::InvalidLevel`] when it is not.
pub fn check_level(level: i32) -> Result<(), CodecError> {
    if (min_c_level()..=max_c_level()).contains(&level) {
        Ok(())
    } else {
        Err(CodecError::InvalidLevel(level))
    }
}

/// A prepared decompression dictionary. Level independent; one per dictionary id is enough.
///
/// Immutable, `Send + Sync`: share it as `Arc<DecoderDict>`. It does not keep the source bytes.
pub struct DecoderDict {
    id: u32,
    dec: DecoderDictionary<'static>,
}

impl DecoderDict {
    /// Prepares the formatted dictionary `bytes` for decompression.
    ///
    /// # Errors
    ///
    /// [`CodecError::Dict`] for a bad header, [`CodecError::InvalidDictionary`] if zstd refuses the
    /// dictionary or its id differs from the header's.
    pub fn new(bytes: &[u8]) -> Result<DecoderDict, CodecError> {
        let id = DictHeader::parse(bytes)?.dict_id;
        let dec = DecoderDictionary::try_copy(bytes).map_err(|_| CodecError::InvalidDictionary)?;
        if dec.as_ddict().get_dict_id().map(|n| n.get()) != Some(id) {
            return Err(CodecError::InvalidDictionary);
        }
        Ok(DecoderDict { id, dec })
    }

    /// The dictionary id (never 0).
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// zstd's own size of the prepared form, for a byte-weighted cache. Saturates at `u64::MAX`.
    #[must_use]
    pub fn mem_size(&self) -> u64 {
        u64::try_from(self.dec.as_ddict().sizeof()).unwrap_or(u64::MAX)
    }
}

/// A prepared compression dictionary for one level.
///
/// Immutable, `Send + Sync`: share it as `Arc<EncoderDict>`. It does not keep the source bytes.
pub struct EncoderDict {
    id: u32,
    level: i32,
    enc: EncoderDictionary<'static>,
}

impl EncoderDict {
    /// Prepares the formatted dictionary `bytes` for compression at `level`.
    ///
    /// # Errors
    ///
    /// [`CodecError::Dict`] for a bad header, [`CodecError::InvalidLevel`] for a level outside
    /// zstd's range, [`CodecError::InvalidDictionary`] if zstd refuses the dictionary or its id
    /// differs from the header's.
    pub fn new(bytes: &[u8], level: i32) -> Result<EncoderDict, CodecError> {
        let id = DictHeader::parse(bytes)?.dict_id;
        check_level(level)?;
        let enc =
            EncoderDictionary::try_copy(bytes, level).map_err(|_| CodecError::InvalidDictionary)?;
        if enc.as_cdict().get_dict_id().map(|n| n.get()) != Some(id) {
            return Err(CodecError::InvalidDictionary);
        }
        Ok(EncoderDict { id, level, enc })
    }

    /// The dictionary id (never 0).
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The compression level this dictionary was prepared for.
    #[must_use]
    pub fn level(&self) -> i32 {
        self.level
    }

    /// zstd's own size of the prepared form, for a byte-weighted cache. Saturates at `u64::MAX`.
    #[must_use]
    pub fn mem_size(&self) -> u64 {
        u64::try_from(self.enc.as_cdict().sizeof()).unwrap_or(u64::MAX)
    }
}

/// A reusable compression context. `Send`, used by one thread at a time (`&mut self`).
///
/// It keeps no reference to a dictionary between calls, so one context (or one pool) serves every
/// dictionary.
pub struct Compressor {
    cctx: CCtx<'static>,
}

impl Compressor {
    /// Creates a context.
    ///
    /// # Errors
    ///
    /// [`CodecError::ContextAlloc`] if zstd cannot allocate it.
    pub fn new() -> Result<Compressor, CodecError> {
        Ok(Compressor {
            cctx: CCtx::try_create().ok_or(CodecError::ContextAlloc)?,
        })
    }

    /// Compresses `src` into one complete frame carrying `dict`'s id and the content size, without
    /// a checksum. `out` is overwritten (not appended to) and reserved to `compress_bound`.
    ///
    /// # Errors
    ///
    /// [`CodecError::Zstd`] on a zstd failure.
    pub fn compress(
        &mut self,
        src: &[u8],
        dict: &EncoderDict,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        out.clear();
        out.reserve(compress_bound(src.len()));
        self.cctx
            .compress_using_cdict(out, src, dict.enc.as_cdict())
            .map_err(zerr)?;
        Ok(())
    }

    /// Compresses `src` into one complete frame without dictionary (`dict_id = 0`), with the
    /// content size and no checksum. `out` is overwritten (not appended to).
    ///
    /// # Errors
    ///
    /// [`CodecError::InvalidLevel`], or [`CodecError::Zstd`] on a zstd failure.
    pub fn compress_plain(
        &mut self,
        src: &[u8],
        level: i32,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        check_level(level)?;
        // Resetting the parameters also drops any dictionary left by a previous call.
        self.cctx
            .reset(ResetDirective::SessionAndParameters)
            .map_err(zerr)?;
        self.cctx
            .set_parameter(CParameter::CompressionLevel(level))
            .map_err(zerr)?;
        self.cctx
            .set_parameter(CParameter::ContentSizeFlag(true))
            .map_err(zerr)?;
        self.cctx
            .set_parameter(CParameter::ChecksumFlag(false))
            .map_err(zerr)?;
        out.clear();
        out.reserve(compress_bound(src.len()));
        self.cctx.compress2(out, src).map_err(zerr)?;
        Ok(())
    }
}

/// A reusable decompression context. `Send`, used by one thread at a time (`&mut self`).
pub struct Decompressor {
    dctx: DCtx<'static>,
}

impl Decompressor {
    /// Creates a context.
    ///
    /// # Errors
    ///
    /// [`CodecError::ContextAlloc`] if zstd cannot allocate it.
    pub fn new() -> Result<Decompressor, CodecError> {
        Ok(Decompressor {
            dctx: DCtx::try_create().ok_or(CodecError::ContextAlloc)?,
        })
    }

    /// Decompresses `frame`, which must be exactly one frame, into `out` (overwritten, not
    /// appended to), producing at most `cap` bytes.
    ///
    /// The frame's dictionary id must equal `dict`'s id, or 0 when `dict` is `None`. A declared
    /// content size above `cap` is rejected before any allocation; a frame without content size
    /// is decoded in streaming mode with a window at most `max(cap, 1024)` and allocation at most
    /// `cap + 1`. A content checksum, if present, is verified. On error the content of `out` is
    /// unspecified.
    ///
    /// The cap bounds what this function **allocates**, not what it writes: if `out` already has a
    /// capacity above `cap + 1`, zstd may fill up to that capacity before the cap check fails. Pass
    /// a buffer whose capacity is not much larger than `cap` to bound the memory touched as well.
    /// `cap = u64::MAX` means "no cap".
    ///
    /// # Errors
    ///
    /// [`CodecError::Frame`], [`CodecError::DictIdMismatch`], [`CodecError::ContentTooLarge`],
    /// [`CodecError::WindowTooLarge`], [`CodecError::OutputTooLarge`], [`CodecError::Zstd`].
    pub fn decompress(
        &mut self,
        frame: &[u8],
        dict: Option<&DecoderDict>,
        cap: u64,
        out: &mut Vec<u8>,
    ) -> Result<FrameInfo, CodecError> {
        let info = inspect_single_frame(frame)?;
        let expected = dict.map_or(0, DecoderDict::id);
        if info.dict_id != expected {
            return Err(CodecError::DictIdMismatch {
                frame: info.dict_id,
                expected,
            });
        }
        out.clear();
        match info.content_size {
            Some(n) => {
                if n > cap {
                    return Err(CodecError::ContentTooLarge { size: n, cap });
                }
                out.reserve_exact(usize::try_from(n).map_err(|_| CodecError::SizeOverflow)?);
                match dict {
                    Some(d) => self
                        .dctx
                        .decompress_using_ddict(out, frame, d.dec.as_ddict()),
                    None => self.dctx.decompress(out, frame),
                }
                .map_err(zerr)?;
                if to_u64(out.len())? != n {
                    return Err(CodecError::Zstd("content size mismatch"));
                }
            }
            None => Self::decompress_stream(&info, frame, dict, cap, out)?,
        }
        Ok(info)
    }

    /// Unknown content size: streaming decode into `out`, never allocating beyond `cap + 1`.
    fn decompress_stream(
        info: &FrameInfo,
        frame: &[u8],
        dict: Option<&DecoderDict>,
        cap: u64,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        if info.window_size > cap.max(1024) {
            return Err(CodecError::WindowTooLarge {
                window: info.window_size,
                cap,
            });
        }
        // `u64::MAX` means "no cap": saturate instead of failing (the known-size path accepts it).
        let limit = usize::try_from(cap.saturating_add(1)).unwrap_or(usize::MAX);
        // A local context: it borrows the dictionary for this call only.
        let mut dctx = DCtx::try_create().ok_or(CodecError::ContextAlloc)?;
        if let Some(d) = dict {
            dctx.ref_ddict(d.dec.as_ddict()).map_err(zerr)?;
        }
        let mut input = InBuffer::around(frame);
        loop {
            if out.len() == out.capacity() {
                let target = out.capacity().saturating_mul(2).max(MIN_GROW).min(limit);
                // `out.len() <= cap` here (checked below after every call), so `target > len`.
                out.reserve_exact(target.saturating_sub(out.len()));
            }
            let (in_before, out_before) = (input.pos(), out.len());
            let mut ob = OutBuffer::around_pos(out, out_before);
            let hint = dctx.decompress_stream(&mut ob, &mut input).map_err(zerr)?;
            if to_u64(out.len())? > cap {
                return Err(CodecError::OutputTooLarge { cap });
            }
            if hint == 0 {
                return Ok(());
            }
            if input.pos() == in_before && out.len() == out_before {
                return Err(CodecError::Zstd("truncated frame"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, OnceLock};
    use zstd::zstd_safe::train_from_buffer;

    fn records(n: usize) -> Vec<Vec<u8>> {
        let mut x: u64 = 7;
        (0..n)
            .map(|i| {
                let mut s = format!(
                    "WARC/1.1\r\nWARC-Type: response\r\nWARC-Target-URI: https://example.com/p/{i}\r\n\
                     Content-Type: application/http; msgtype=response\r\n\r\nHTTP/1.1 200 OK\r\n\r\n"
                )
                .into_bytes();
                for _ in 0..200 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    s.push(b'a' + u8::try_from(x % 26).unwrap());
                }
                s
            })
            .collect()
    }

    fn base_dict() -> &'static [u8] {
        static D: OnceLock<Vec<u8>> = OnceLock::new();
        D.get_or_init(|| {
            let recs = records(2000);
            let sizes: Vec<usize> = recs.iter().map(Vec::len).collect();
            let data: Vec<u8> = recs.concat();
            let mut dict = Vec::with_capacity(16 * 1024);
            train_from_buffer(&mut dict, &data, &sizes).unwrap();
            dict
        })
    }

    fn test_dict(id: u32) -> Vec<u8> {
        let mut d = base_dict().to_vec();
        crate::set_dict_id(&mut d, id).unwrap();
        d
    }

    fn pair(id: u32) -> (EncoderDict, DecoderDict) {
        let b = test_dict(id);
        (
            EncoderDict::new(&b, 3).unwrap(),
            DecoderDict::new(&b).unwrap(),
        )
    }

    fn zeros(n: usize) -> Vec<u8> {
        vec![0u8; n]
    }

    /// Frame from a raw CCtx with the content size left out of the header.
    fn unknown_size_frame(src: &[u8], window_log: u32, dict: Option<&[u8]>) -> Vec<u8> {
        let cd = dict.map(|d| zstd::zstd_safe::CDict::create(d, 3));
        let mut c = CCtx::create();
        c.set_parameter(CParameter::ContentSizeFlag(false)).unwrap();
        c.set_parameter(CParameter::WindowLog(window_log)).unwrap();
        if let Some(cd) = cd.as_ref() {
            c.ref_cdict(cd).unwrap();
        }
        let mut out = Vec::with_capacity(compress_bound(src.len()));
        c.compress2(&mut out, src).unwrap();
        out
    }

    #[test]
    fn roundtrip_with_dict() {
        let (e, d) = pair(40_000);
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        for r in records(20) {
            let mut f = Vec::new();
            c.compress(&r, &e, &mut f).unwrap();
            let i = inspect_single_frame(&f).unwrap();
            assert_eq!(i.dict_id, 40_000);
            assert_eq!(i.content_size, Some(r.len() as u64));
            assert!(!i.has_checksum);
            let mut back = Vec::new();
            let info = dc.decompress(&f, Some(&d), 1 << 20, &mut back).unwrap();
            assert_eq!(info, i);
            assert_eq!(back, r);
        }
    }

    #[test]
    fn empty_record_roundtrip() {
        let (e, d) = pair(40_000);
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        let mut f = Vec::new();
        c.compress(&[], &e, &mut f).unwrap();
        let mut back = vec![1, 2, 3];
        dc.decompress(&f, Some(&d), 10, &mut back).unwrap();
        assert!(back.is_empty());
        c.compress_plain(&[], 3, &mut f).unwrap();
        dc.decompress(&f, None, 0, &mut back).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn dict_id_field_sizes_real() {
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        for (id, did_flag) in [(200u32, 1u8), (40_000, 2), (100_000, 3)] {
            let (e, d) = pair(id);
            let r = &records(1)[0];
            let mut f = Vec::new();
            c.compress(r, &e, &mut f).unwrap();
            assert_eq!(f[4] & 3, did_flag, "id {id}");
            assert_eq!(inspect_frame_id(&f), id);
            let mut back = Vec::new();
            dc.decompress(&f, Some(&d), 1 << 20, &mut back).unwrap();
            assert_eq!(&back, r);
        }
    }

    fn inspect_frame_id(f: &[u8]) -> u32 {
        inspect_single_frame(f).unwrap().dict_id
    }

    #[test]
    fn roundtrip_plain() {
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        let r = records(1).remove(0);
        let mut f = Vec::new();
        c.compress_plain(&r, 5, &mut f).unwrap();
        let i = inspect_single_frame(&f).unwrap();
        assert_eq!(
            (i.dict_id, i.content_size, i.has_checksum),
            (0, Some(r.len() as u64), false)
        );
        let mut back = Vec::new();
        dc.decompress(&f, None, 1 << 20, &mut back).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn plain_after_dict_same_ctx() {
        let (e, d) = pair(40_000);
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        let r = records(1).remove(0);
        let (mut f1, mut f2, mut f3) = (Vec::new(), Vec::new(), Vec::new());
        c.compress(&r, &e, &mut f1).unwrap();
        c.compress_plain(&r, 3, &mut f2).unwrap();
        c.compress(&r, &e, &mut f3).unwrap();
        assert_eq!(inspect_frame_id(&f1), 40_000);
        assert_eq!(inspect_frame_id(&f2), 0);
        assert_eq!(inspect_frame_id(&f3), 40_000);
        let mut back = Vec::new();
        for (f, dict) in [(&f1, Some(&d)), (&f2, None), (&f3, Some(&d))] {
            dc.decompress(f, dict, 1 << 20, &mut back).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn compressor_reused_across_dicts() {
        let (ea, da) = pair(40_000);
        let (eb, db) = pair(50_000);
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        let recs = records(8);
        let mut f = Vec::new();
        let mut back = Vec::new();
        for (i, r) in recs.iter().enumerate() {
            let (e, d) = if i % 2 == 0 { (&ea, &da) } else { (&eb, &db) };
            c.compress(r, e, &mut f).unwrap();
            assert_eq!(inspect_frame_id(&f), e.id());
            dc.decompress(&f, Some(d), 1 << 20, &mut back).unwrap();
            assert_eq!(&back, r);
        }
    }

    #[test]
    fn encoder_levels_share_decoder() {
        let b = test_dict(40_000);
        let d = DecoderDict::new(&b).unwrap();
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        let r = records(1).remove(0);
        for level in [1, 19] {
            let e = EncoderDict::new(&b, level).unwrap();
            assert_eq!(e.level(), level);
            let mut f = Vec::new();
            c.compress(&r, &e, &mut f).unwrap();
            assert_eq!(inspect_frame_id(&f), 40_000);
            let mut back = Vec::new();
            dc.decompress(&f, Some(&d), 1 << 20, &mut back).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn ids_agree() {
        let b = test_dict(123_456);
        let h = DictHeader::parse(&b).unwrap().dict_id;
        assert_eq!(EncoderDict::new(&b, 3).unwrap().id(), h);
        assert_eq!(DecoderDict::new(&b).unwrap().id(), h);
        assert_eq!(h, 123_456);
    }

    #[test]
    fn mem_size_nonzero() {
        let b = test_dict(40_000);
        let d = DecoderDict::new(&b).unwrap();
        assert!(d.mem_size() >= u64::try_from(b.len() / 2).unwrap());
        let e1 = EncoderDict::new(&b, 1).unwrap();
        let e19 = EncoderDict::new(&b, 19).unwrap();
        assert!(e19.mem_size() > e1.mem_size());
    }

    #[test]
    fn dict_id_mismatch() {
        let (ea, _) = pair(40_000);
        let (_, db) = pair(50_000);
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        let r = records(1).remove(0);
        let mut f = Vec::new();
        let mut back = Vec::new();
        c.compress(&r, &ea, &mut f).unwrap();
        assert!(matches!(
            dc.decompress(&f, Some(&db), 1 << 20, &mut back),
            Err(CodecError::DictIdMismatch {
                frame: 40_000,
                expected: 50_000
            })
        ));
        assert!(matches!(
            dc.decompress(&f, None, 1 << 20, &mut back),
            Err(CodecError::DictIdMismatch {
                frame: 40_000,
                expected: 0
            })
        ));
        c.compress_plain(&r, 3, &mut f).unwrap();
        assert!(matches!(
            dc.decompress(&f, Some(&db), 1 << 20, &mut back),
            Err(CodecError::DictIdMismatch {
                frame: 0,
                expected: 50_000
            })
        ));
    }

    #[test]
    fn content_too_large_no_alloc() {
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        let mut f = Vec::new();
        c.compress_plain(&zeros(1 << 20), 3, &mut f).unwrap();
        let mut out = Vec::new();
        assert!(matches!(
            dc.decompress(&f, None, 64 * 1024, &mut out),
            Err(CodecError::ContentTooLarge { size, cap }) if size == 1 << 20 && cap == 64 * 1024
        ));
        assert_eq!(out.capacity(), 0);
    }

    #[test]
    fn unknown_size_roundtrip() {
        let b = test_dict(40_000);
        let d = DecoderDict::new(&b).unwrap();
        let r = records(1).remove(0);
        let mut dc = Decompressor::new().unwrap();
        let mut back = Vec::new();
        let f = unknown_size_frame(&r, 17, None);
        let i = dc.decompress(&f, None, 1 << 20, &mut back).unwrap();
        assert_eq!(i.content_size, None);
        assert_eq!(back, r);
        let f = unknown_size_frame(&r, 17, Some(&b));
        let i = dc.decompress(&f, Some(&d), 1 << 20, &mut back).unwrap();
        assert_eq!((i.content_size, i.dict_id), (None, 40_000));
        assert_eq!(back, r);
        // a larger input exercises buffer growth
        let big: Vec<u8> = records(2000).concat();
        let f = unknown_size_frame(&big, 20, None);
        dc.decompress(&f, None, 1 << 24, &mut back).unwrap();
        assert_eq!(back, big);
    }

    #[test]
    fn unknown_size_cap() {
        let f = unknown_size_frame(&zeros(4 << 20), 20, None);
        assert_eq!(inspect_single_frame(&f).unwrap().content_size, None);
        let mut dc = Decompressor::new().unwrap();
        let mut out = Vec::new();
        let cap = 2u64 << 20;
        assert!(matches!(
            dc.decompress(&f, None, cap, &mut out),
            Err(CodecError::OutputTooLarge { .. })
        ));
        assert!(out.capacity() as u64 <= cap + 1);
    }

    #[test]
    fn unknown_size_window_too_large() {
        let f = unknown_size_frame(&zeros(4 << 20), 20, None);
        let mut dc = Decompressor::new().unwrap();
        let mut out = Vec::new();
        assert!(matches!(
            dc.decompress(&f, None, 64 * 1024, &mut out),
            Err(CodecError::WindowTooLarge { .. })
        ));
        assert_eq!(out.capacity(), 0);
    }

    #[test]
    fn exact_cap_ok() {
        let mut dc = Decompressor::new().unwrap();
        let mut c = Compressor::new().unwrap();
        let src = zeros(1 << 20);
        let mut out = Vec::new();
        let mut f = Vec::new();
        c.compress_plain(&src, 3, &mut f).unwrap();
        dc.decompress(&f, None, 1 << 20, &mut out).unwrap();
        assert_eq!(out, src);
        let f = unknown_size_frame(&src, 20, None);
        dc.decompress(&f, None, 1 << 20, &mut out).unwrap();
        assert_eq!(out, src);
        assert!(matches!(
            dc.decompress(&f, None, (1 << 20) - 1, &mut out),
            Err(CodecError::OutputTooLarge { .. } | CodecError::WindowTooLarge { .. })
        ));
    }

    #[test]
    fn delegated_checksum_frame() {
        let mut x: u64 = 99;
        let src: Vec<u8> = (0..1000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x.to_le_bytes()[0]
            })
            .collect();
        let mut cc = CCtx::create();
        cc.set_parameter(CParameter::ChecksumFlag(true)).unwrap();
        let mut f = Vec::with_capacity(compress_bound(src.len()));
        cc.compress2(&mut f, &src).unwrap();
        assert!(inspect_single_frame(&f).unwrap().has_checksum);
        let mut dc = Decompressor::new().unwrap();
        let mut out = Vec::new();
        dc.decompress(&f, None, 1 << 20, &mut out).unwrap();
        assert_eq!(out, src);
        let mid = f.len() / 2;
        f[mid] ^= 0x40;
        assert!(dc.decompress(&f, None, 1 << 20, &mut out).is_err());
        // the context is still usable
        let mut f2 = Vec::new();
        Compressor::new()
            .unwrap()
            .compress_plain(&src, 3, &mut f2)
            .unwrap();
        dc.decompress(&f2, None, 1 << 20, &mut out).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn corrupt_frame_errors() {
        let (e, d) = pair(40_000);
        let r = records(1).remove(0);
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        let mut f = Vec::new();
        c.compress(&r, &e, &mut f).unwrap();
        let mut out = Vec::new();
        for i in 0..f.len() {
            for flip in [0x01u8, 0x80, 0xFF] {
                let mut g = f.clone();
                g[i] ^= flip;
                // Ok (different bytes) or Err; never a panic.
                let _ = dc.decompress(&g, Some(&d), 1 << 20, &mut out);
            }
        }
        // still healthy
        dc.decompress(&f, Some(&d), 1 << 20, &mut out).unwrap();
        assert_eq!(out, r);
    }

    #[test]
    fn trailing_frame_rejected() {
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        let mut f = Vec::new();
        c.compress_plain(b"hello", 3, &mut f).unwrap();
        let mut two = f.clone();
        two.extend_from_slice(&f);
        let mut out = Vec::new();
        assert!(matches!(
            dc.decompress(&two, None, 1 << 20, &mut out),
            Err(CodecError::Frame(FrameError::TrailingBytes { .. }))
        ));
    }

    #[test]
    fn decoder_dict_new_errors() {
        let good = test_dict(40_000);
        let mut bad_magic = good.clone();
        bad_magic[0] ^= 1;
        assert!(matches!(
            DecoderDict::new(&bad_magic),
            Err(CodecError::Dict(DictError::BadMagic(_)))
        ));
        let mut zero = good.clone();
        zero[4..8].fill(0);
        assert!(matches!(
            DecoderDict::new(&zero),
            Err(CodecError::Dict(DictError::ZeroId))
        ));
        let mut corrupt = good.clone();
        corrupt[8..64].fill(0xFF);
        assert!(matches!(
            DecoderDict::new(&corrupt),
            Err(CodecError::InvalidDictionary)
        ));
        assert!(matches!(
            DecoderDict::new(&good[..8]),
            Err(CodecError::InvalidDictionary)
        ));
    }

    #[test]
    fn encoder_dict_new_errors() {
        let good = test_dict(40_000);
        let mut bad_magic = good.clone();
        bad_magic[0] ^= 1;
        assert!(matches!(
            EncoderDict::new(&bad_magic, 3),
            Err(CodecError::Dict(DictError::BadMagic(_)))
        ));
        let mut zero = good.clone();
        zero[4..8].fill(0);
        assert!(matches!(
            EncoderDict::new(&zero, 3),
            Err(CodecError::Dict(DictError::ZeroId))
        ));
        let mut corrupt = good.clone();
        corrupt[8..64].fill(0xFF);
        assert!(matches!(
            EncoderDict::new(&corrupt, 3),
            Err(CodecError::InvalidDictionary)
        ));
        assert!(matches!(
            EncoderDict::new(&good, 23),
            Err(CodecError::InvalidLevel(23))
        ));
        assert!(matches!(
            Compressor::new()
                .unwrap()
                .compress_plain(b"x", 23, &mut Vec::new()),
            Err(CodecError::InvalidLevel(23))
        ));
    }

    #[test]
    fn send_sync() {
        fn ss<T: Send + Sync>() {}
        fn s<T: Send>() {}
        ss::<DecoderDict>();
        ss::<EncoderDict>();
        s::<Compressor>();
        s::<Decompressor>();
    }

    #[test]
    fn arc_shared_across_threads() {
        let (e, d) = pair(40_000);
        let (e, d) = (Arc::new(e), Arc::new(d));
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let (e, d) = (Arc::clone(&e), Arc::clone(&d));
                std::thread::spawn(move || {
                    let mut c = Compressor::new().unwrap();
                    let mut dc = Decompressor::new().unwrap();
                    let mut f = Vec::new();
                    let mut back = Vec::new();
                    for r in records(200 + t) {
                        c.compress(&r, &e, &mut f).unwrap();
                        assert_eq!(inspect_single_frame(&f).unwrap().dict_id, 40_000);
                        dc.decompress(&f, Some(&d), 1 << 20, &mut back).unwrap();
                        assert_eq!(back, r);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
