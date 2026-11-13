//! Zstd formatted-dictionary header: magic and dictionary id (RFC 8878 §5).

/// Magic number of a formatted zstd dictionary (stored little-endian in the first 4 bytes).
pub const DICT_MAGIC: u32 = 0xEC30_A437;

/// Bytes of magic + dictionary id at the start of a formatted dictionary.
pub const DICT_HEADER_LEN: usize = 8;

/// First dictionary id Masstuffy may allocate (zstd reserves `0..=32767` for public registration).
pub const DICT_ID_ALLOC_MIN: u32 = 32_768;

/// End (exclusive) of the allocatable id range (zstd reserves ids `>= 2^31`).
pub const DICT_ID_ALLOC_END: u32 = 1 << 31;

/// Whether `id` lies in `[DICT_ID_ALLOC_MIN, DICT_ID_ALLOC_END)`, the range Masstuffy allocates from.
#[must_use]
pub fn is_allocatable_dict_id(id: u32) -> bool {
    (DICT_ID_ALLOC_MIN..DICT_ID_ALLOC_END).contains(&id)
}

/// The header of a formatted zstd dictionary.
///
/// Invariant: `dict_id != 0` (id 0 means "no dictionary" in a frame header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictHeader {
    /// Dictionary id stored in bytes `4..8`.
    pub dict_id: u32,
}

impl DictHeader {
    /// Parses the first [`DICT_HEADER_LEN`] bytes of `bytes`.
    ///
    /// Raw-content dictionaries (no magic) are rejected: Masstuffy needs the id inside the
    /// dictionary. The entropy tables are not validated here (zstd does when loading).
    ///
    /// # Errors
    ///
    /// [`DictError::Truncated`] under 8 bytes, [`DictError::BadMagic`] if the magic differs,
    /// [`DictError::ZeroId`] if the id is 0.
    pub fn parse(bytes: &[u8]) -> Result<DictHeader, DictError> {
        let magic = read_u32(bytes, 0).ok_or(DictError::Truncated)?;
        let dict_id = read_u32(bytes, 4).ok_or(DictError::Truncated)?;
        if magic != DICT_MAGIC {
            return Err(DictError::BadMagic(magic));
        }
        if dict_id == 0 {
            return Err(DictError::ZeroId);
        }
        Ok(DictHeader { dict_id })
    }
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let raw: [u8; 4] = bytes.get(at..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

/// Rewrites the dictionary id of a formatted dictionary in place.
///
/// The header is validated first, so only valid dictionaries are patched; `bytes` is unchanged on
/// error. The allocation range is not enforced (see [`is_allocatable_dict_id`]). No checksum covers
/// bytes `4..8`, so the patched dictionary stays valid for zstd.
///
/// # Errors
///
/// Any error of [`DictHeader::parse`], and [`DictError::ZeroId`] if `id` is 0.
pub fn set_dict_id(bytes: &mut [u8], id: u32) -> Result<(), DictError> {
    DictHeader::parse(bytes)?;
    if id == 0 {
        return Err(DictError::ZeroId);
    }
    let field = bytes.get_mut(4..8).ok_or(DictError::Truncated)?;
    field.copy_from_slice(&id.to_le_bytes());
    Ok(())
}

/// Error from dictionary header handling.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DictError {
    /// Fewer than [`DICT_HEADER_LEN`] bytes.
    #[error("dictionary header truncated")]
    Truncated,
    /// The first four bytes are not the zstd dictionary magic.
    #[error("bad dictionary magic {0:#010x}")]
    BadMagic(u32),
    /// The dictionary id is 0.
    #[error("dictionary id is zero")]
    ZeroId,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(id: u32) -> Vec<u8> {
        let mut v = DICT_MAGIC.to_le_bytes().to_vec();
        v.extend_from_slice(&id.to_le_bytes());
        v.extend_from_slice(&[1, 2, 3, 4]);
        v
    }

    #[test]
    fn parse_ok() {
        let h = DictHeader::parse(&header(0x1234_5678)).unwrap();
        assert_eq!(h.dict_id, 0x1234_5678);
    }

    #[test]
    fn parse_truncated_every_len() {
        let h = header(7);
        for n in 0..DICT_HEADER_LEN {
            assert_eq!(DictHeader::parse(&h[..n]), Err(DictError::Truncated));
        }
        assert!(DictHeader::parse(&h[..DICT_HEADER_LEN]).is_ok());
    }

    #[test]
    fn parse_bad_magic() {
        let mut h = header(7);
        h[0] ^= 1;
        assert!(matches!(DictHeader::parse(&h), Err(DictError::BadMagic(_))));
    }

    #[test]
    fn parse_zero_id() {
        assert_eq!(DictHeader::parse(&header(0)), Err(DictError::ZeroId));
    }

    #[test]
    fn set_dict_id_roundtrip() {
        let mut h = header(7);
        let before = h.clone();
        set_dict_id(&mut h, 40_000).unwrap();
        assert_eq!(DictHeader::parse(&h).unwrap().dict_id, 40_000);
        assert_eq!(h[..4], before[..4]);
        assert_eq!(h[8..], before[8..]);
        assert_eq!(h[4..8], 40_000u32.to_le_bytes());
    }

    #[test]
    fn set_dict_id_rejects_zero_and_bad_magic() {
        let mut h = header(7);
        let before = h.clone();
        assert_eq!(set_dict_id(&mut h, 0), Err(DictError::ZeroId));
        assert_eq!(h, before);
        h[1] ^= 1;
        let before = h.clone();
        assert!(matches!(
            set_dict_id(&mut h, 9),
            Err(DictError::BadMagic(_))
        ));
        assert_eq!(h, before);
        let mut short = [0u8; 3];
        assert_eq!(set_dict_id(&mut short, 9), Err(DictError::Truncated));
    }

    #[test]
    fn alloc_range_bounds() {
        assert!(!is_allocatable_dict_id(0));
        assert!(!is_allocatable_dict_id(32_767));
        assert!(is_allocatable_dict_id(32_768));
        assert!(is_allocatable_dict_id((1 << 31) - 1));
        assert!(!is_allocatable_dict_id(1 << 31));
    }
}
