//! Strict, zero-copy WARC/1.0 and WARC/1.1 record parser and serialiser.
//!
//! The version is kept byte-exactly, and the builder writes 1.1 unless [`RecordBuilder::version`]
//! says otherwise.
//!
//! * [`RecordRef`] is a borrowed, validated view of one record (hot path: one record per read).
//! * [`Record`] and [`RecordBuilder`] build and own serialized records; `Content-Length` is always
//!   computed by the builder.
//! * [`WarcReader`] streams concatenated records from any [`std::io::BufRead`].
//!
//! Parsing rejects malformed input with a [`ParseError`], is bounded by [`Limits`], never panics and
//! never allocates an attacker-controlled size up front.
//!
//! ```
//! use masstuffy_warc::{Limits, RecordBuilder, RecordRef};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut b = RecordBuilder::new("resource")?;
//! b.record_id("urn:uuid:00000000-0000-4000-8000-000000000000")?
//!     .date("2026-11-12T10:00:00Z".parse()?)
//!     .target_uri("http://example.org/")?;
//! let record = b.build(b"hello")?;
//!
//! let view = RecordRef::parse(record.as_bytes(), &Limits::DEFAULT)?;
//! assert_eq!(view.record_id(), Some("urn:uuid:00000000-0000-4000-8000-000000000000"));
//! assert_eq!(view.body(), b"hello");
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod date;
mod error;
mod header;
mod limits;
mod parse;
mod reader;
mod record;
#[cfg(test)]
mod testutil;
mod version;

pub use date::WarcDate;
pub use error::{BuildError, ParseError, ReadError};
pub use header::{
    CONTENT_LENGTH, Header, Headers, WARC_DATE, WARC_RECORD_ID, WARC_TARGET_URI, WARC_TYPE,
};
pub use limits::Limits;
pub use parse::RecordRef;
pub use reader::WarcReader;
pub use record::{Record, RecordBuilder};
pub use version::Version;
