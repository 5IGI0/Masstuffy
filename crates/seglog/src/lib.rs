//! Append-only segmented byte log with positioned reads (spec 02 "Segments", 04 §4.2c/§4.3, 05).
//!
//! A log lives in one directory as files `<stem>.<N>.<ext>`, `N` from 1 without gaps. The caller
//! picks `stem` and `ext` ([`Naming`]); this crate knows nothing about WARC, zstd or indexes.
//!
//! # Blocking, Unix only
//!
//! Everything here is blocking `std::fs` I/O (and uses `FileExt` positioned reads, so Unix only).
//! Async callers must call it inside `spawn_blocking`.
//!
//! # Single writer
//!
//! One [`Appender`] per log at a time. This crate does not lock: the caller guarantees it (for
//! example by holding a `masstuffy_durable::DirLock`). Any number of [`Reader`] clones may read
//! concurrently, also while an `Appender` appends.
//!
//! # Rollover rule
//!
//! A batch of `total` bytes goes to the current segment of size `size` unless
//! `size > 0 && size + total > threshold`, in which case it goes to the start of a fresh segment.
//! Consequently a batch is never split (I9), a segment of exactly `threshold` bytes is allowed, an
//! empty current segment takes any batch (an oversize batch is written alone, without an extra
//! empty file), and a segment exceeds `threshold` only if it holds exactly one batch. This uses `>`
//! where the spec says `>=` (decision confirmed by the user, see `docs/architecture.md` §5).
//!
//! # Durability
//!
//! [`Appender::append_batch`] makes bytes visible to readers but not durable. [`Appender::sync`]
//! fsyncs the data of every touched segment, then the directory if a segment file was created.
//! Bytes written but not synced are an orphan or torn tail after a crash (I3): [`Appender::open`]
//! continues at the real file length. Any I/O error poisons the `Appender`: drop it and `open`
//! again, and treat every locator returned since the last successful `sync` as lost (re-append
//! it): a reopened `Appender` only fsyncs its last segment (and the directory), never data in
//! earlier segments.
//!
//! # Removal
//!
//! [`remove_all`] deletes a whole log. Readers keep serving already-cached handles of removed
//! files; call [`Reader::forget_all`] to release them.
#![forbid(unsafe_code)]

mod appender;
mod error;
mod naming;
mod reader;
mod remove;

#[cfg(test)]
mod testutil;

use std::path::{Path, PathBuf};

pub use appender::Appender;
pub use error::{Error, Op};
pub use naming::Naming;
pub use reader::{DEFAULT_MAX_OPEN_FILES, DEFAULT_MAX_READ_LEN, Reader, ReaderOptions};
pub use remove::remove_all;

/// Where a batch item lives: `len` bytes at `offset` in segment `segment` (>= 1).
///
/// Fields are public because locators come back from an index (untrusted): [`Reader`] validates
/// them, not this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Locator {
    /// Segment number, from 1.
    pub segment: u32,
    /// Byte offset in the segment.
    pub offset: u64,
    /// Length in bytes.
    pub len: u64,
}

impl Locator {
    /// `offset + len`, `None` on overflow.
    #[must_use]
    pub fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.len)
    }
}

/// Path of segment `n`. Called only on create, rollover, open and cache miss.
pub(crate) fn segment_path(dir: &Path, naming: &Naming, n: u32) -> PathBuf {
    dir.join(format!("{}.{}.{}", naming.stem(), n, naming.ext()))
}
