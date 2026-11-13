//! Error types of the crate.

use crate::legacy::LegacyError;

/// A semantically invalid [`Entry`](crate::Entry).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EntryError {
    /// A required text field is empty (the field name).
    #[error("empty field `{0}`")]
    Empty(&'static str),
    /// `warc_type` or `record_id` contains a byte `<= 0x20` or `0x7F` (the field name).
    #[error("invalid byte in field `{0}`")]
    InvalidAtom(&'static str),
    /// `raw_size` is zero.
    #[error("raw_size is zero")]
    ZeroRawSize,
    /// `offset + raw_size` overflows `u64`.
    #[error("offset + raw_size overflows u64")]
    LocatorOverflow,
    /// The encoded line (newline included) is longer than the cap.
    #[error("line of {len} bytes exceeds the cap of {limit}")]
    LineTooLong {
        /// Length of the encoded line.
        len: usize,
        /// The cap.
        limit: usize,
    },
}

/// A bad header line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HeaderError {
    /// The line does not start with `#masstuffy-cdx/`.
    #[error("not a masstuffy-cdx header")]
    NotCdx,
    /// A well-formed version this crate does not know.
    #[error("unsupported masstuffy-cdx version {version}")]
    UnsupportedVersion {
        /// The version found.
        version: u32,
    },
    /// The header does not match the strict grammar.
    #[error("invalid masstuffy-cdx header")]
    Invalid,
}

/// A bad entry line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LineError {
    /// The line reaches end of input without a newline (reader only).
    #[error("incomplete line")]
    Incomplete,
    /// The line is longer than the cap (reader only).
    #[error("line longer than {limit} bytes")]
    TooLong {
        /// The cap.
        limit: usize,
    },
    /// The check field is missing, malformed or does not match.
    #[error("line checksum missing or wrong")]
    Checksum,
    /// The content is not UTF-8.
    #[error("line is not valid UTF-8")]
    NotUtf8,
    /// The line does not have exactly 10 fields.
    #[error("wrong number of fields")]
    FieldCount,
    /// The kind field is neither `A` nor `T`.
    #[error("invalid kind")]
    InvalidKind,
    /// A number is not canonical decimal or is out of range (the field name).
    #[error("invalid number in field `{0}`")]
    InvalidNumber(&'static str),
    /// The digest is not 16 lowercase hex digits.
    #[error("invalid digest")]
    InvalidDigest,
    /// The date is invalid or not in canonical form.
    #[error("invalid date")]
    InvalidDate,
    /// The url field has an invalid escape.
    #[error("invalid escape in url")]
    InvalidEscape,
    /// The fields are well-formed but the entry is invalid.
    #[error(transparent)]
    Entry(#[from] EntryError),
}

impl LineError {
    /// Whether a torn write can produce this error (see the reader's torn-tail rules).
    pub(crate) fn is_torn_candidate(&self) -> bool {
        matches!(
            self,
            Self::Checksum | Self::Incomplete | Self::TooLong { .. }
        )
    }
}

/// Errors of [`CdxReader`](crate::CdxReader).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadError {
    /// I/O error from the underlying reader.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// End of input before a complete header line.
    #[error("missing header line")]
    MissingHeader,
    /// The header line is bad.
    #[error("bad header: {0}")]
    Header(#[from] HeaderError),
    /// Corruption that is not a torn tail, starting at byte `at`.
    #[error("corrupt index at byte {at}: {source}")]
    Corrupt {
        /// Offset of the first bad line.
        at: u64,
        /// What is wrong with it.
        #[source]
        source: LineError,
    },
    /// A stream offset overflowed `u64`.
    #[error("stream offset overflow")]
    OffsetOverflow,
    /// `max_line` is smaller than the longest header line.
    #[error("max_line {0} is too small")]
    InvalidMaxLine(usize),
    /// An earlier error left the reader unusable.
    #[error("reader is poisoned by an earlier error")]
    Poisoned,
}

/// Errors of [`CdxWriter`](crate::CdxWriter).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WriteError {
    /// I/O error from the underlying writer.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// An earlier write failed.
    #[error("writer is poisoned by an earlier error")]
    Poisoned,
}

/// Errors of [`EnvelopeReader`](crate::EnvelopeReader).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnvelopeError {
    /// `EnvelopeLimits::max_line` is below the 64-byte minimum.
    #[error("max_line {0} is below the 64-byte minimum")]
    InvalidMaxLine(usize),
    /// I/O error from the underlying reader.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The optional header line is bad.
    #[error("bad header: {0}")]
    Header(#[from] HeaderError),
    /// A bad `#masstuffy-cdx/1` entry line.
    #[error("bad line at byte {offset}: {source}")]
    Line {
        /// Offset of the line.
        offset: u64,
        /// What is wrong with it.
        #[source]
        source: LineError,
    },
    /// A bad legacy line.
    #[error("bad legacy line at byte {offset}: {source}")]
    Legacy {
        /// Offset of the line.
        offset: u64,
        /// What is wrong with it.
        #[source]
        source: LegacyError,
    },
    /// A line longer than the cap.
    #[error("line at byte {offset} longer than {limit} bytes")]
    LineTooLong {
        /// Offset of the line.
        offset: u64,
        /// The cap.
        limit: usize,
    },
    /// The stream ends inside a line or a payload.
    #[error("truncated input in the record at byte {offset}")]
    Truncated {
        /// Offset of the record's line.
        offset: u64,
    },
    /// `raw_size` exceeds the cap; no payload byte was read.
    #[error("record at byte {offset} claims {size} bytes, limit is {limit}")]
    RecordTooLarge {
        /// Offset of the record's line.
        offset: u64,
        /// The claimed size.
        size: u64,
        /// The cap.
        limit: u64,
    },
    /// A tombstone line in an import.
    #[error("tombstone in import at byte {offset}")]
    Tombstone {
        /// Offset of the line.
        offset: u64,
    },
    /// A stream offset overflowed `u64`.
    #[error("stream offset overflow")]
    OffsetOverflow,
    /// An earlier error left the reader unusable.
    #[error("reader is poisoned by an earlier error")]
    Poisoned,
}
