//! Error types of the crate.

/// Why a byte sequence is not a valid WARC/1.1 record.
///
/// Offsets are byte offsets from the start of the record.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    /// The input ends before the record is complete.
    #[error("input ends before the record is complete")]
    Truncated,
    /// The first line is not exactly `WARC/1.1\r\n`.
    #[error("first line is not exactly `WARC/1.1\\r\\n`")]
    InvalidVersion,
    /// A line is terminated by a bare LF.
    #[error("bare CR or LF at offset {offset}")]
    InvalidLineEnding {
        /// Offset of the offending LF.
        offset: usize,
    },
    /// The header section is larger than `Limits::max_header_bytes`.
    #[error("header section exceeds {limit} bytes")]
    HeaderSectionTooLarge {
        /// The cap that was exceeded.
        limit: usize,
    },
    /// More header lines than `Limits::max_headers`.
    #[error("more than {limit} header lines")]
    TooManyHeaders {
        /// The cap that was exceeded.
        limit: usize,
    },
    /// A header line has no `": "` separator.
    #[error("header line without `: ` at offset {offset}")]
    MalformedHeaderLine {
        /// Offset of the start of the line.
        offset: usize,
    },
    /// A header name is empty or contains a non-token byte.
    #[error("invalid header name at offset {offset}")]
    InvalidHeaderName {
        /// Offset of the start of the line.
        offset: usize,
    },
    /// A header value contains a forbidden byte or is not valid UTF-8.
    #[error("invalid header value at offset {offset}")]
    InvalidHeaderValue {
        /// Offset of the offending byte (or of the line for semantic errors).
        offset: usize,
    },
    /// A mandatory header is absent.
    #[error("missing mandatory header {0}")]
    MissingHeader(&'static str),
    /// A header that must be unique appears twice.
    #[error("header {0} appears more than once")]
    DuplicateHeader(&'static str),
    /// `Content-Length` is not a canonical unsigned decimal fitting in a `u64`.
    #[error("invalid Content-Length")]
    InvalidContentLength,
    /// `Content-Length` is larger than `Limits::max_body_len`.
    #[error("Content-Length {length} exceeds limit {limit}")]
    BodyTooLarge {
        /// Declared length.
        length: u64,
        /// The cap that was exceeded.
        limit: u64,
    },
    /// `WARC-Record-ID` is not a `<uri>`.
    #[error("invalid WARC-Record-ID")]
    InvalidRecordId,
    /// `WARC-Date` is not a W3C-ISO8601 UTC timestamp.
    #[error("invalid WARC-Date")]
    InvalidDate,
    /// The body is not followed by `\r\n\r\n`.
    #[error("record not followed by `\\r\\n\\r\\n`")]
    InvalidTrailer,
    /// `RecordRef::parse` found bytes after the record.
    #[error("{extra} unexpected bytes after the record")]
    TrailingData {
        /// Number of extra bytes.
        extra: usize,
    },
}

/// Why a record cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    /// The header name is empty or contains a non-token byte.
    #[error("invalid header name")]
    InvalidHeaderName,
    /// The header value is not acceptable for this header.
    #[error("invalid value for header {name}")]
    InvalidHeaderValue {
        /// Name of the header.
        name: String,
    },
    /// `Content-Length` cannot be set by the caller.
    #[error("Content-Length is computed by the builder and cannot be set")]
    ReservedHeader,
    /// The record id is not a valid URI.
    #[error("invalid WARC-Record-ID")]
    InvalidRecordId,
    /// A mandatory header is absent.
    #[error("missing mandatory header {0}")]
    MissingHeader(&'static str),
    /// The record size does not fit in memory addressing.
    #[error("record size overflows")]
    TooLarge,
    /// The system clock is outside 1970..=9999.
    #[error("system clock outside 1970..=9999")]
    ClockOutOfRange,
}

/// Error of `WarcReader::next_record`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadError {
    /// The underlying reader failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The stream is not valid WARC.
    #[error(transparent)]
    Parse(#[from] ParseError),
    /// A previous call failed, the stream position is unknown.
    #[error("a previous call failed; the reader cannot resynchronise")]
    Poisoned,
}
