//! Error type.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// The operation that failed in an [`Error::Io`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op {
    /// Creating a segment file.
    Create,
    /// Opening a segment file.
    Open,
    /// Reading a directory.
    ReadDir,
    /// `stat` / `fstat` / file type.
    Stat,
    /// Seeking.
    Seek,
    /// Writing.
    Write,
    /// `fdatasync` on a segment.
    SyncData,
    /// Reading segment bytes.
    Read,
    /// Removing a segment file.
    Remove,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Op::Create => "create",
            Op::Open => "open",
            Op::ReadDir => "read directory",
            Op::Stat => "stat",
            Op::Seek => "seek",
            Op::Write => "write",
            Op::SyncData => "fsync data",
            Op::Read => "read",
            Op::Remove => "remove",
        };
        f.write_str(s)
    }
}

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An I/O call failed. `path` is the file or directory the failing call acted on.
    #[error("{op} failed on {}", path.display())]
    Io {
        /// The failing operation.
        op: Op,
        /// The path acted on.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Directory fsync (or another durable primitive) failed.
    #[error(transparent)]
    Durable(#[from] masstuffy_durable::Error),
    /// `Naming::new`: empty part, `/` or NUL, or a stem starting with `.`.
    #[error("invalid segment naming (stem {stem:?}, ext {ext:?})")]
    InvalidNaming {
        /// The rejected stem.
        stem: String,
        /// The rejected extension.
        ext: String,
    },
    /// `Appender::create`: a segment of this log already exists.
    #[error("segment log already exists: {}", path.display())]
    AlreadyExists {
        /// The existing segment.
        path: PathBuf,
    },
    /// `Appender::open`: no segment of this log in `dir`.
    #[error("no segment {stem}.<N>.{ext} in {}", dir.display())]
    NoSegments {
        /// The directory.
        dir: PathBuf,
        /// The stem searched.
        stem: String,
        /// The extension searched.
        ext: String,
    },
    /// `Appender::open`: segments are not exactly `1..=max`.
    #[error("segment numbering has gaps in {}: {count} segments, highest {max}", dir.display())]
    Gap {
        /// The directory.
        dir: PathBuf,
        /// Number of segments found.
        count: u64,
        /// Highest segment number found.
        max: u32,
    },
    /// An entry looks like a segment of this log but its number is malformed (leading zero, 0,
    /// or above `u32::MAX`).
    #[error("malformed segment name {}", path.display())]
    BadSegmentName {
        /// The entry.
        path: PathBuf,
    },
    /// A segment name is not a regular file (directory, symlink, ...).
    #[error("segment is not a regular file: {}", path.display())]
    NotAFile {
        /// The entry.
        path: PathBuf,
    },
    /// Rolling over would need a segment number above `u32::MAX`.
    #[error("segment number limit reached")]
    SegmentLimit,
    /// `append_batch`: item `index` is empty.
    #[error("batch item {index} is empty")]
    EmptyItem {
        /// Index of the first empty item.
        index: usize,
    },
    /// `append_batch`: batch size or resulting segment size overflows `u64`.
    #[error("batch too large")]
    BatchTooLarge,
    /// A previous I/O error left the `Appender` in an unknown state; drop it and `open` again.
    #[error("appender is poisoned by a previous I/O error")]
    Poisoned,
    /// Reader: the segment file does not exist (or segment 0).
    #[error("segment {segment} not found")]
    SegmentNotFound {
        /// The segment number.
        segment: u32,
    },
    /// Reader: `[offset, offset+len)` is not inside the segment (or overflows `u64`).
    #[error("read out of bounds: segment {segment}, offset {offset}, len {len}")]
    OutOfBounds {
        /// The segment number.
        segment: u32,
        /// The requested offset.
        offset: u64,
        /// The requested length.
        len: u64,
    },
    /// Reader: `len` exceeds `max_read_len` or `usize`.
    #[error("read of {len} bytes exceeds the limit {max}")]
    ReadTooLarge {
        /// The requested length.
        len: u64,
        /// The limit in force.
        max: u64,
    },
}

impl Error {
    /// The `io::ErrorKind`, for `Io` and for `Durable` wrapping an I/O error.
    #[must_use]
    pub fn io_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Error::Io { source, .. } => Some(source.kind()),
            Error::Durable(e) => e.io_kind(),
            _ => None,
        }
    }
}

/// Builds an `Io` error mapper for `map_err`.
pub(crate) fn io(op: Op, path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { op, path, source }
}
