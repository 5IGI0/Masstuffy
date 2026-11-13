//! Repository-wide zstd dictionary store (spec 2 "Dictionary store", 3.3, 4.4).
//!
//! The store is one directory of files named `<label>.<dict_id>.zstdict`. [`DictStore::open`] scans
//! it (headers only), [`DictStore::install`] adds a trained dictionary under a fresh id, and
//! [`DictStore::decoder`] / [`DictStore::encoder`] return prepared dictionaries from a cache bounded
//! in bytes.
//!
//! # Blocking
//!
//! Everything here is synchronous: file I/O, `fsync` and zstd preparation. Async callers wrap `open`,
//! `install` and any `decoder`/`encoder` call that may miss in `spawn_blocking`.
//!
//! # Never deletes
//!
//! No function of this crate deletes, renames or rewrites an existing dictionary file (invariant
//! I5: segments may reference any id forever).
//!
//! # Locks
//!
//! Lock order: `install_lock` then `files`. The cache lock is never held together with another lock,
//! nor during I/O or dictionary preparation.
#![forbid(unsafe_code)]

mod cache;
mod error;
mod file;
mod install;
mod name;
mod store;
#[cfg(test)]
mod testutil;

pub use error::{Error, Op};
pub use file::DictInfo;
pub use install::Installed;
pub use masstuffy_zframe::{DecoderDict, EncoderDict};
pub use name::is_valid_label;
pub use store::{CacheStats, DictStore, ScanReport};

/// File extension of dictionary files.
pub const EXT: &str = "zstdict";
/// Longest accepted label, in bytes.
pub const MAX_LABEL_LEN: usize = 128;
/// Default cap on a dictionary file size (64 MiB).
pub const DEFAULT_MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;
/// Default byte budget of the prepared-dictionary cache (256 MiB).
pub const DEFAULT_CACHE_BYTES: u64 = 256 * 1024 * 1024;
/// At most this many ignored names are kept in a [`ScanReport`] (the count is exact).
pub const MAX_REPORTED_IGNORED: usize = 64;

/// Limits of a [`DictStore`]. Not validated: `cache_bytes = 0` caches nothing, and
/// `max_file_size < 8` makes every file too large.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Largest dictionary file accepted (scan, load, install). Files are never read past it.
    pub max_file_size: u64,
    /// Byte budget of the decoder and encoder cache (sum of `mem_size()` of cached entries).
    ///
    /// It should exceed the largest prepared dictionary: a larger one is returned but not cached,
    /// so it is rebuilt on every call (callers should then hold the `Arc`). The budget bounds what
    /// the cache retains, not total memory: an `Arc` handed out keeps its dictionary alive after
    /// eviction.
    pub cache_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            cache_bytes: DEFAULT_CACHE_BYTES,
        }
    }
}
