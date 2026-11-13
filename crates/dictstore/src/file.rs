//! Dictionary files: header read and bounded full read.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use masstuffy_zframe::{DICT_HEADER_LEN, DictError, DictHeader};

use crate::error::{Error, Op, io as io_err};

/// One dictionary file of the store. Immutable once created; shared as `Arc<DictInfo>`.
#[derive(Debug)]
pub struct DictInfo {
    pub(crate) dict_id: u32,
    pub(crate) label: Box<str>,
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
}

impl DictInfo {
    /// The dictionary id (equal to the id in the file name and in the header; never 0).
    #[must_use]
    pub fn dict_id(&self) -> u32 {
        self.dict_id
    }

    /// The label from the file name (valid per [`is_valid_label`](crate::is_valid_label)).
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Path of the file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// File length at scan or install time (at most `Options::max_file_size`).
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// Opens `path`, checks it is a regular file of at most `max` bytes, returns it with its length.
fn open_checked(path: &Path, max: u64) -> Result<(File, u64), Error> {
    let file = File::open(path).map_err(io_err(Op::Open, path))?;
    let meta = file.metadata().map_err(io_err(Op::Stat, path))?;
    if !meta.is_file() {
        return Err(Error::NotAFile {
            path: path.to_owned(),
        });
    }
    let len = meta.len();
    if len > max {
        return Err(Error::FileTooLarge {
            path: path.to_owned(),
            size: len,
            max,
        });
    }
    Ok((file, len))
}

/// Reads only the 8-byte header of `path`. Returns `(file length, header id)`.
pub(crate) fn read_header(path: &Path, max: u64) -> Result<(u64, u32), Error> {
    let (mut file, len) = open_checked(path, max)?;
    let mut buf = [0u8; DICT_HEADER_LEN];
    file.read_exact(&mut buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::BadHeader {
                path: path.to_owned(),
                source: DictError::Truncated,
            }
        } else {
            io_err(Op::Read, path)(e)
        }
    })?;
    let header = DictHeader::parse(&buf).map_err(|source| Error::BadHeader {
        path: path.to_owned(),
        source,
    })?;
    Ok((len, header.dict_id))
}

/// Reads the whole file of `info`, never more than `max + 1` bytes of memory, and checks its
/// header id against the id of `info` (catches a file swapped behind the store).
pub(crate) fn read_full(info: &DictInfo, max: u64) -> Result<Vec<u8>, Error> {
    let path = info.path.as_path();
    let (file, len) = open_checked(path, max)?;
    let too_large = |size| Error::FileTooLarge {
        path: path.to_owned(),
        size,
        max,
    };
    let cap = usize::try_from(len).map_err(|_| too_large(len))?;
    let mut buf = Vec::with_capacity(cap);
    file.take(max.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(io_err(Op::Read, path))?;
    let got = u64::try_from(buf.len()).unwrap_or(u64::MAX);
    if got > max {
        return Err(too_large(got));
    }
    let header = DictHeader::parse(&buf).map_err(|source| Error::BadHeader {
        path: path.to_owned(),
        source,
    })?;
    if header.dict_id != info.dict_id {
        return Err(Error::IdMismatch {
            path: path.to_owned(),
            name_id: info.dict_id,
            header_id: header.dict_id,
        });
    }
    Ok(buf)
}

/// Whether `fs::symlink_metadata(path)` succeeds (the entry exists, whatever it is).
pub(crate) fn exists_nofollow(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
