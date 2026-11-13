//! Error type.

use std::fmt;
use std::io;
use std::path::PathBuf;

use masstuffy_zframe::{CodecError, DictError};

/// The operation that failed in an [`Error::Io`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op {
    /// Reading the store directory.
    ReadDir,
    /// `stat` (or `lstat`).
    Stat,
    /// Opening a dictionary file.
    Open,
    /// Reading a dictionary file.
    Read,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Op::ReadDir => "read directory",
            Op::Stat => "stat",
            Op::Open => "open",
            Op::Read => "read",
        };
        f.write_str(s)
    }
}

/// Errors of this crate. Every variant that concerns a file names its path.
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
    /// `sweep_tmp`, `atomic_write` or `sync_dir` failed.
    #[error(transparent)]
    Durable(#[from] masstuffy_durable::Error),
    /// A store-named entry is not a regular file (directory, symlink, FIFO...).
    #[error("dictionary is not a regular file: {}", path.display())]
    NotAFile {
        /// The entry.
        path: PathBuf,
    },
    /// A dictionary file exceeds `Options::max_file_size`.
    #[error("dictionary file {} is {size} bytes, above the cap {max}", path.display())]
    FileTooLarge {
        /// The file.
        path: PathBuf,
        /// Its size (at least; reads stop past the cap).
        size: u64,
        /// The cap.
        max: u64,
    },
    /// `install` input exceeds `Options::max_file_size`.
    #[error("dictionary of {size} bytes exceeds the cap {max}")]
    InputTooLarge {
        /// Input length.
        size: u64,
        /// The cap.
        max: u64,
    },
    /// A store-named file has a malformed header (truncated, bad magic, id 0).
    #[error("bad dictionary header in {}: {source}", path.display())]
    BadHeader {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        #[source]
        source: DictError,
    },
    /// The id in the file name differs from the id in the header.
    #[error("dictionary {} has id {header_id} in its header but {name_id} in its name", path.display())]
    IdMismatch {
        /// The file.
        path: PathBuf,
        /// Id from the name.
        name_id: u32,
        /// Id from the header.
        header_id: u32,
    },
    /// Two files carry the same id (invariant I6). `first < second` in path order.
    #[error("duplicate dictionary id {dict_id}: {} and {}", first.display(), second.display())]
    DuplicateId {
        /// The id.
        dict_id: u32,
        /// The smaller path.
        first: PathBuf,
        /// The larger path.
        second: PathBuf,
    },
    /// No dictionary with this id (including id 0).
    #[error("no dictionary with id {dict_id}")]
    NotFound {
        /// The id.
        dict_id: u32,
    },
    /// `install`: the label does not satisfy `is_valid_label`.
    #[error("invalid dictionary label {label:?}")]
    InvalidLabel {
        /// The rejected label.
        label: String,
    },
    /// `install`: the input is not a formatted dictionary (raw-content dictionaries are rejected).
    #[error("invalid dictionary input: {0}")]
    InvalidInput(#[source] DictError),
    /// zstd refused the dictionary or the level (install validation, or preparation on load).
    /// `dict_id` is the header id of the input for `install`.
    #[error("dictionary {dict_id} rejected: {source}")]
    Codec {
        /// The dictionary id.
        dict_id: u32,
        /// zstd's complaint.
        #[source]
        source: CodecError,
    },
    /// `install`: the target file name exists although its id is not in the store (a file added
    /// behind the store's back). It is never overwritten (I5).
    #[error("dictionary file already exists: {}", path.display())]
    AlreadyExists {
        /// The existing file.
        path: PathBuf,
    },
    /// `install`: no free id (unreachable: the store holds far fewer than 2^31 - 32768 ids).
    #[error("no free dictionary id")]
    NoFreeId,
}

/// Builds an `Io` error mapper for `map_err`.
pub(crate) fn io(op: Op, path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { op, path, source }
}
