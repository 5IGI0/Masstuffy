//! Resource caps applied while parsing untrusted input.

/// Caps applied by the parser and the reader.
///
/// Header sizes are in-memory quantities (`usize`); the body length is a `u64` as it comes from the
/// `Content-Length` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Max bytes of version line + header lines + empty line. Default 64 KiB.
    pub max_header_bytes: usize,
    /// Max number of header lines. Default 256.
    pub max_headers: usize,
    /// Max accepted `Content-Length`. Default 1 GiB.
    pub max_body_len: u64,
}

impl Limits {
    /// 64 KiB of header bytes, 256 headers, 1 GiB body.
    pub const DEFAULT: Limits = Limits {
        max_header_bytes: 64 * 1024,
        max_headers: 256,
        max_body_len: 1 << 30,
    };

    /// No caps. Only for already-trusted bytes (e.g. `Record::view`).
    pub const UNLIMITED: Limits = Limits {
        max_header_bytes: usize::MAX,
        max_headers: usize::MAX,
        max_body_len: u64::MAX,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}
