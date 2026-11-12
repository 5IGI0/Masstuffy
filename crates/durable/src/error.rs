//! Error type.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The operation that failed in an [`Error::Io`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op {
    /// Creating the temporary file.
    CreateTmp,
    /// Writing file contents.
    Write,
    /// `fsync` on a file.
    SyncFile,
    /// `rename`.
    Rename,
    /// Opening a directory.
    OpenDir,
    /// `fsync` on a directory.
    SyncDir,
    /// Opening the lock file.
    OpenLock,
    /// Taking the lock.
    Lock,
    /// `stat`.
    Stat,
    /// Reading a directory.
    ReadDir,
    /// Removing an entry.
    Remove,
    /// Removing a trashed directory.
    RemoveTrash,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Op::CreateTmp => "create temp file",
            Op::Write => "write",
            Op::SyncFile => "fsync file",
            Op::Rename => "rename",
            Op::OpenDir => "open directory",
            Op::SyncDir => "fsync directory",
            Op::OpenLock => "open lock file",
            Op::Lock => "lock",
            Op::Stat => "stat",
            Op::ReadDir => "read directory",
            Op::Remove => "remove",
            Op::RemoveTrash => "remove trash",
        };
        f.write_str(s)
    }
}

/// Errors of this crate. Every variant names the path it concerns.
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
    /// [`DirLock::try_acquire`](crate::DirLock::try_acquire): the lock is held elsewhere (this or
    /// another process).
    #[error("directory {} is already locked (lock file {})", dir.display(), lock_file.display())]
    Locked {
        /// The directory.
        dir: PathBuf,
        /// Its lock file.
        lock_file: PathBuf,
    },
    /// The file name is empty, `.`/`..`, contains `/` or NUL, or is reserved.
    #[error("invalid file name {name:?} in {}", dir.display())]
    InvalidName {
        /// The directory.
        dir: PathBuf,
        /// The rejected name.
        name: OsString,
    },
}

impl Error {
    /// The path the error concerns (`dir` for `Locked` and `InvalidName`).
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Error::Io { path, .. } => path,
            Error::Locked { dir, .. } | Error::InvalidName { dir, .. } => dir,
        }
    }

    /// The failing operation, only for `Io`.
    #[must_use]
    pub fn op(&self) -> Option<Op> {
        match self {
            Error::Io { op, .. } => Some(*op),
            _ => None,
        }
    }

    /// The `io::ErrorKind`, only for `Io`.
    #[must_use]
    pub fn io_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Error::Io { source, .. } => Some(source.kind()),
            _ => None,
        }
    }
}

/// Builds an `Io` error mapper for `map_err`.
pub(crate) fn io(op: Op, path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { op, path, source }
}
