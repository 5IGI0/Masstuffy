//! Crash-safe filesystem primitives: atomic file replace, directory fsync, an exclusive directory
//! lock and atomic directory deletion (spec 05 §5.2 and §5.3).
//!
//! # Blocking
//!
//! Every function here is blocking `std::fs` I/O and `fsync` can take tens of milliseconds. Async
//! callers must call them inside `spawn_blocking`.
//!
//! # Guarantees and assumptions
//!
//! [`atomic_write`] never exposes a torn file: a reader, or the disk after a crash, sees the old or
//! the new content, complete. `Ok(())` means the new content is durable. This relies on POSIX
//! `rename(2)` atomicity within one filesystem, `fsync` persisting file data and inode, `fsync` on a
//! directory persisting its entries, and the device honouring flushes. These are assumed (ext4, xfs on
//! Linux), not guaranteed on NFS, FUSE or with barriers disabled.
//!
//! # Reserved names
//!
//! `.lock` ([`LOCK_FILE`]), `.tmp-*` ([`TMP_PREFIX`]) and `.trash-*` ([`TRASH_PREFIX`]) are owned by
//! this crate. Directory scanners in higher crates must skip them ([`is_reserved`]).
//!
//! # Startup sweeps
//!
//! [`sweep_tmp`] and [`sweep_trash`] delete other writers' in-flight files: call them only at
//! startup, while holding the repository [`DirLock`], before any writer starts.
#![forbid(unsafe_code)]

mod atomic;
mod error;
mod lock;
mod trash;
mod unique;

#[cfg(test)]
mod testutil;

use std::ffi::OsStr;

pub use atomic::{atomic_write, sync_dir};
pub use error::{Error, Op};
pub use lock::DirLock;
pub use trash::{sweep_tmp, sweep_trash, trash_dir};

/// Name of the lock file taken by [`DirLock`].
pub const LOCK_FILE: &str = ".lock";
/// Prefix of in-flight or crashed [`atomic_write`] temp files.
pub const TMP_PREFIX: &str = ".tmp-";
/// Prefix of directories being deleted by [`trash_dir`].
pub const TRASH_PREFIX: &str = ".trash-";

/// True iff `name` is `.lock` or starts with `.tmp-` or `.trash-`.
///
/// Directory scanners (recovery, dictionary store scan) must skip such entries.
#[must_use]
pub fn is_reserved(name: &OsStr) -> bool {
    let b = name.as_encoded_bytes();
    b == LOCK_FILE.as_bytes()
        || b.starts_with(TMP_PREFIX.as_bytes())
        || b.starts_with(TRASH_PREFIX.as_bytes())
}
