//! Zstd frame header inspection (RFC 8878 §3.1.1). Std only, allocation free, never decodes.

/// Magic number of a standard zstd frame (stored little-endian).
pub const FRAME_MAGIC: u32 = 0xFD2F_B528;

/// Largest frame header: 4 magic + 1 descriptor + 1 window + 4 dictionary id + 8 content size.
pub const MAX_FRAME_HEADER_LEN: usize = 18;

/// Largest legal block content size (RFC 8878 `Block_Maximum_Size`).
const BLOCK_MAX: u64 = 128 * 1024;

/// What the header of a zstd frame says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    /// Dictionary id from the header; 0 when the field is absent or holds 0 (meaning "no dictionary").
    pub dict_id: u32,
    /// `Frame_Content_Size`, if present.
    pub content_size: Option<u64>,
    /// Window size in bytes; for single-segment frames, equal to the content size.
    pub window_size: u64,
    /// The `Single_Segment_flag`.
    pub single_segment: bool,
    /// The frame ends with a 4-byte content checksum.
    pub has_checksum: bool,
    /// Bytes of magic + frame header (6..=18).
    pub header_len: usize,
}

/// Error from frame inspection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
    /// The buffer ends before the end of the header (or of the frame, for `inspect_single_frame`).
    #[error("zstd frame truncated")]
    Truncated,
    /// The first four bytes are not a zstd frame magic.
    #[error("bad zstd frame magic {0:#010x}")]
    BadMagic(u32),
    /// A skippable frame (magic `0x184D2A5x`); not accepted as a record frame.
    #[error("skippable zstd frame")]
    SkippableFrame,
    /// A legacy (pre-v0.8) frame; not supported.
    #[error("legacy zstd frame")]
    LegacyFrame,
    /// The reserved bit of the frame header descriptor is set.
    #[error("reserved bit set in zstd frame header")]
    ReservedBit,
    /// A block uses the reserved block type.
    #[error("reserved zstd block type at offset {offset}")]
    ReservedBlockType {
        /// Offset of the block header from the start of the buffer.
        offset: u64,
    },
    /// A block declares a size above 128 KiB.
    #[error("zstd block larger than 128 KiB at offset {offset}")]
    BlockTooLarge {
        /// Offset of the block header from the start of the buffer.
        offset: u64,
    },
    /// Bytes follow the end of the first frame.
    #[error("trailing bytes after the zstd frame (frame length {frame_len})")]
    TrailingBytes {
        /// Length of the first frame in bytes.
        frame_len: u64,
    },
}

fn le_u64(buf: &[u8], at: usize, len: usize) -> Option<u64> {
    let end = at.checked_add(len)?;
    let bytes = buf.get(at..end)?;
    let mut v = 0u64;
    for (i, b) in bytes.iter().enumerate() {
        let shift = u32::try_from(i).ok()?.checked_mul(8)?;
        v |= u64::from(*b).checked_shl(shift)?;
    }
    Some(v)
}

/// Parses the header of the first zstd frame in `buf`.
///
/// Only the header is read: `buf` may be a prefix of the frame but must hold at least
/// `header_len` bytes. Nothing is allocated.
///
/// # Errors
///
/// [`FrameError::Truncated`] if the header is incomplete; [`FrameError::BadMagic`],
/// [`FrameError::SkippableFrame`], [`FrameError::LegacyFrame`] for other magics;
/// [`FrameError::ReservedBit`] if the reserved descriptor bit is set.
pub fn inspect_frame(buf: &[u8]) -> Result<FrameInfo, FrameError> {
    let magic = le_u64(buf, 0, 4).ok_or(FrameError::Truncated)?;
    let magic = u32::try_from(magic).map_err(|_| FrameError::Truncated)?;
    match magic {
        FRAME_MAGIC => {}
        0x184D_2A50..=0x184D_2A5F => return Err(FrameError::SkippableFrame),
        0xFD2F_B51E | 0xFD2F_B522..=0xFD2F_B527 => return Err(FrameError::LegacyFrame),
        m => return Err(FrameError::BadMagic(m)),
    }
    let fhd = *buf.get(4).ok_or(FrameError::Truncated)?;
    let fcs_flag = fhd >> 6;
    let single_segment = fhd & 0x20 != 0;
    // Bit 4 is "unused" in the RFC and ignored.
    if fhd & 0x08 != 0 {
        return Err(FrameError::ReservedBit);
    }
    let has_checksum = fhd & 0x04 != 0;
    let did_flag = fhd & 0x03;

    let window_len = usize::from(!single_segment);
    let did_len = match did_flag {
        0 => 0usize,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    let fcs_len = match fcs_flag {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    // Sum of values bounded by 1 + 4 + 8: cannot overflow.
    let header_len = 5 + window_len + did_len + fcs_len;
    if buf.len() < header_len {
        return Err(FrameError::Truncated);
    }

    let window_desc = if single_segment {
        None
    } else {
        Some(*buf.get(5).ok_or(FrameError::Truncated)?)
    };
    let did_at = 5 + window_len;
    let fcs_at = did_at + did_len;
    let dict_id = if did_len == 0 {
        0
    } else {
        let v = le_u64(buf, did_at, did_len).ok_or(FrameError::Truncated)?;
        u32::try_from(v).map_err(|_| FrameError::Truncated)?
    };
    let content_size = if fcs_len == 0 {
        None
    } else {
        let v = le_u64(buf, fcs_at, fcs_len).ok_or(FrameError::Truncated)?;
        if fcs_len == 2 {
            Some(v.checked_add(256).ok_or(FrameError::Truncated)?)
        } else {
            Some(v)
        }
    };
    let window_size = match (window_desc, content_size) {
        (Some(b), _) => {
            // exp <= 31 so log <= 41: base < 2^42 and window < 2^43, no overflow.
            let exp = u32::from(b >> 3);
            let mant = u64::from(b & 7);
            let base = 1u64 << (10 + exp);
            base + (base / 8) * mant
        }
        // Single segment: the content size is always present (fcs_len >= 1).
        (None, cs) => cs.ok_or(FrameError::Truncated)?,
    };
    Ok(FrameInfo {
        dict_id,
        content_size,
        window_size,
        single_segment,
        has_checksum,
        header_len,
    })
}

/// Like [`inspect_frame`], but also walks the block headers and proves that `buf` is exactly one
/// complete frame with nothing after it (no second frame, no skippable frame, no padding).
///
/// Cost is O(number of blocks), no allocation, no decoding: it does not prove the block contents
/// decode (a decompressor does).
///
/// # Errors
///
/// Any error of [`inspect_frame`]; [`FrameError::Truncated`] if the frame is cut;
/// [`FrameError::ReservedBlockType`] / [`FrameError::BlockTooLarge`] for a bad block header;
/// [`FrameError::TrailingBytes`] if `buf` is longer than the frame.
pub fn inspect_single_frame(buf: &[u8]) -> Result<FrameInfo, FrameError> {
    let info = inspect_frame(buf)?;
    let total = u64::try_from(buf.len()).map_err(|_| FrameError::Truncated)?;
    let mut pos = u64::try_from(info.header_len).map_err(|_| FrameError::Truncated)?;
    loop {
        let at = usize::try_from(pos).map_err(|_| FrameError::Truncated)?;
        let h = le_u64(buf, at, 3).ok_or(FrameError::Truncated)?;
        let last = h & 1 != 0;
        let btype = (h >> 1) & 3;
        let bsize = h >> 3;
        if btype == 3 {
            return Err(FrameError::ReservedBlockType { offset: pos });
        }
        if bsize > BLOCK_MAX {
            return Err(FrameError::BlockTooLarge { offset: pos });
        }
        // An RLE block stores one byte whatever its declared size.
        let payload = if btype == 1 { 1 } else { bsize };
        pos = pos
            .checked_add(3)
            .and_then(|p| p.checked_add(payload))
            .ok_or(FrameError::Truncated)?;
        if pos > total {
            return Err(FrameError::Truncated);
        }
        if last {
            break;
        }
    }
    if info.has_checksum {
        pos = pos.checked_add(4).ok_or(FrameError::Truncated)?;
        if pos > total {
            return Err(FrameError::Truncated);
        }
    }
    if pos != total {
        return Err(FrameError::TrailingBytes { frame_len: pos });
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &[u8] = b"hello world";

    fn f_hello() -> Vec<u8> {
        let mut v = vec![0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x0b, 0x59, 0x00, 0x00];
        v.extend_from_slice(HELLO);
        v
    }
    fn f_hello_nofcs_ck() -> Vec<u8> {
        let mut v = vec![0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x00, 0x59, 0x00, 0x00];
        v.extend_from_slice(HELLO);
        v.extend_from_slice(&[0x68, 0x69, 0x1e, 0xb2]);
        v
    }
    fn f_rle76() -> Vec<u8> {
        vec![
            0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x4c, 0x45, 0x00, 0x00, 0x10, 0x61, 0x61, 0x01, 0x00,
            0x87, 0x02, 0x2c,
        ]
    }
    fn f_empty() -> Vec<u8> {
        vec![0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x00, 0x01, 0x00, 0x00]
    }
    fn fixtures() -> Vec<Vec<u8>> {
        vec![f_hello(), f_hello_nofcs_ck(), f_rle76(), f_empty()]
    }

    fn magic_then(magic: u32, rest: &[u8]) -> Vec<u8> {
        let mut v = magic.to_le_bytes().to_vec();
        v.extend_from_slice(rest);
        v
    }

    #[test]
    fn real_frames_fields() {
        let i = inspect_frame(&f_hello()).unwrap();
        assert_eq!(
            i,
            FrameInfo {
                dict_id: 0,
                content_size: Some(11),
                window_size: 11,
                single_segment: true,
                has_checksum: false,
                header_len: 6
            }
        );
        let i = inspect_frame(&f_hello_nofcs_ck()).unwrap();
        assert_eq!(
            i,
            FrameInfo {
                dict_id: 0,
                content_size: None,
                window_size: 1024,
                single_segment: false,
                has_checksum: true,
                header_len: 6
            }
        );
        let i = inspect_frame(&f_rle76()).unwrap();
        assert_eq!(i.content_size, Some(76));
        assert_eq!(i.window_size, 76);
        assert!(i.single_segment && !i.has_checksum);
        assert_eq!((i.dict_id, i.header_len), (0, 6));
        let i = inspect_frame(&f_empty()).unwrap();
        assert_eq!(i.content_size, Some(0));
        assert_eq!(i.window_size, 0);
        assert_eq!((i.dict_id, i.header_len), (0, 6));
        assert!(i.single_segment && !i.has_checksum);
    }

    #[test]
    fn single_frame_exact() {
        for f in fixtures() {
            assert_eq!(
                inspect_single_frame(&f).unwrap(),
                inspect_frame(&f).unwrap()
            );
        }
    }

    #[test]
    fn trailing_bytes() {
        let mut two = f_hello();
        two.extend_from_slice(&f_hello());
        assert_eq!(
            inspect_single_frame(&two),
            Err(FrameError::TrailingBytes { frame_len: 20 })
        );
        let mut one_more = f_hello();
        one_more.push(0);
        assert_eq!(
            inspect_single_frame(&one_more),
            Err(FrameError::TrailingBytes { frame_len: 20 })
        );
    }

    #[test]
    fn truncated_every_len() {
        for f in fixtures() {
            let hl = inspect_frame(&f).unwrap().header_len;
            for n in 0..f.len() {
                let r = inspect_frame(&f[..n]);
                if n >= hl {
                    assert!(r.is_ok(), "len {n}");
                } else {
                    assert_eq!(r, Err(FrameError::Truncated), "len {n}");
                }
                assert_eq!(inspect_single_frame(&f[..n]), Err(FrameError::Truncated));
            }
        }
    }

    #[test]
    fn bad_magic() {
        assert_eq!(
            inspect_frame(&magic_then(0xDEAD_BEEF, &[0x20, 0, 1, 0, 0])),
            Err(FrameError::BadMagic(0xDEAD_BEEF))
        );
        assert_eq!(
            inspect_frame(&magic_then(0, &[0; 8])),
            Err(FrameError::BadMagic(0))
        );
    }

    #[test]
    fn skippable_all_16_magics() {
        for m in 0x184D_2A50..=0x184D_2A5Fu32 {
            assert_eq!(
                inspect_frame(&magic_then(m, &[0; 8])),
                Err(FrameError::SkippableFrame)
            );
        }
        assert!(matches!(
            inspect_frame(&magic_then(0x184D_2A4F, &[0; 8])),
            Err(FrameError::BadMagic(_))
        ));
        assert!(matches!(
            inspect_frame(&magic_then(0x184D_2A60, &[0; 8])),
            Err(FrameError::BadMagic(_))
        ));
    }

    #[test]
    fn legacy_magics() {
        for m in [
            0xFD2F_B51E,
            0xFD2F_B522,
            0xFD2F_B523,
            0xFD2F_B524,
            0xFD2F_B525,
            0xFD2F_B526,
            0xFD2F_B527,
        ] {
            assert_eq!(
                inspect_frame(&magic_then(m, &[0; 8])),
                Err(FrameError::LegacyFrame)
            );
        }
    }

    #[test]
    fn reserved_bit() {
        let f = magic_then(FRAME_MAGIC, &[0x28, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(inspect_frame(&f), Err(FrameError::ReservedBit));
        // bit 4 is ignored
        let f = magic_then(FRAME_MAGIC, &[0x30, 0x00, 0x01, 0x00, 0x00]);
        assert!(inspect_frame(&f).is_ok());
    }

    #[test]
    fn reserved_block_type() {
        // block header: last=1, type=3, size=0 -> 0b111 = 7
        let f = magic_then(FRAME_MAGIC, &[0x20, 0x00, 0x07, 0x00, 0x00]);
        assert_eq!(
            inspect_single_frame(&f),
            Err(FrameError::ReservedBlockType { offset: 6 })
        );
    }

    #[test]
    fn block_too_large() {
        // raw last block of 128 KiB + 1: header only, error before the payload is needed
        let h = ((128 * 1024 + 1) << 3) | 1u32;
        let b = h.to_le_bytes();
        let f = magic_then(FRAME_MAGIC, &[0x20, 0x00, b[0], b[1], b[2]]);
        assert_eq!(
            inspect_single_frame(&f),
            Err(FrameError::BlockTooLarge { offset: 6 })
        );
    }

    #[test]
    fn dict_id_field_sizes() {
        // single segment, fcs flag 0 (1 byte), did flag n
        let f = magic_then(FRAME_MAGIC, &[0x21, 0xAB, 0x05]);
        let i = inspect_frame(&f).unwrap();
        assert_eq!(
            (i.dict_id, i.header_len, i.content_size),
            (0xAB, 7, Some(5))
        );
        let f = magic_then(FRAME_MAGIC, &[0x22, 0xCD, 0xAB, 0x05]);
        let i = inspect_frame(&f).unwrap();
        assert_eq!((i.dict_id, i.header_len), (0xABCD, 8));
        let f = magic_then(FRAME_MAGIC, &[0x23, 0xEF, 0xCD, 0xAB, 0x89, 0x05]);
        let i = inspect_frame(&f).unwrap();
        assert_eq!((i.dict_id, i.header_len), (0x89AB_CDEF, 10));
        let f = magic_then(FRAME_MAGIC, &[0x21, 0x00, 0x05]);
        assert_eq!(inspect_frame(&f).unwrap().dict_id, 0);
    }

    #[test]
    fn fcs_field_sizes() {
        let f = magic_then(FRAME_MAGIC, &[0x60, 0xFF, 0xFF]);
        let i = inspect_frame(&f).unwrap();
        assert_eq!(
            (i.content_size, i.header_len, i.window_size),
            (Some(65_791), 7, 65_791)
        );
        let f = magic_then(FRAME_MAGIC, &[0xA0, 1, 2, 3, 4]);
        assert_eq!(inspect_frame(&f).unwrap().content_size, Some(0x0403_0201));
        let mut rest = vec![0xE0];
        rest.extend_from_slice(&[0xFF; 8]);
        let i = inspect_frame(&magic_then(FRAME_MAGIC, &rest)).unwrap();
        assert_eq!((i.content_size, i.header_len), (Some(u64::MAX), 13));
        // not single segment, flag 0: no content size
        let f = magic_then(FRAME_MAGIC, &[0x00, 0x00]);
        let i = inspect_frame(&f).unwrap();
        assert_eq!((i.content_size, i.header_len), (None, 6));
        // largest header
        let mut rest = vec![0xC3, 0x00, 1, 2, 3, 4];
        rest.extend_from_slice(&[0; 8]);
        let i = inspect_frame(&magic_then(FRAME_MAGIC, &rest)).unwrap();
        assert_eq!(i.header_len, MAX_FRAME_HEADER_LEN);
    }

    #[test]
    fn window_descriptor() {
        let w = |b: u8| {
            inspect_frame(&magic_then(FRAME_MAGIC, &[0x00, b]))
                .unwrap()
                .window_size
        };
        assert_eq!(w(0x00), 1024);
        assert_eq!(w(0x48), 512 * 1024);
        assert_eq!(w(0x50), 1 << 20);
        assert_eq!(w(0x51), (1 << 20) + (128 << 10));
        assert_eq!(w(0xFF), (1u64 << 41) + 7 * (1u64 << 38));
    }

    #[test]
    fn no_panic_random() {
        let mut x: u64 = 1;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for i in 0..20_000 {
            let len = usize::try_from(next() % 41).unwrap();
            let mut buf: Vec<u8> = (0..len).map(|_| next().to_le_bytes()[0]).collect();
            if i % 2 == 0 && buf.len() >= 4 {
                buf[..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
            }
            let _ = inspect_frame(&buf);
            let _ = inspect_single_frame(&buf);
        }
    }
}
