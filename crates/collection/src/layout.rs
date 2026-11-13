//! File names of a collection directory and creation of a generation's CDX.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use masstuffy_cdx::{CdxWriter, DEFAULT_MAX_LINE};
use masstuffy_seglog::Naming;

use crate::error::Error;
use crate::manifest::Codec;

/// Name of the manifest file.
pub(crate) const MANIFEST_NAME: &str = "manifest.json";

/// Segment naming of generation `gen`: `seg.<gen>.<N>.warc` or `.warc.zst`.
pub(crate) fn segment_naming(generation: u32, codec: &Codec) -> Result<Naming, Error> {
    let ext = match codec {
        Codec::None => "warc",
        Codec::Zstd { .. } => "warc.zst",
    };
    Ok(Naming::new(format!("seg.{generation}"), ext)?)
}

/// Name of the CDX of generation `gen`.
pub(crate) fn cdx_name(generation: u32) -> String {
    format!("index.{generation}.cdx")
}

/// Creates `index.<gen>.cdx` with its header line, fsyncs the file then `dir`.
///
/// The file is opened in append mode, so the returned writer appends after the header.
pub(crate) fn create_cdx(dir: &Path, generation: u32) -> Result<CdxWriter<File>, Error> {
    let path = dir.join(cdx_name(generation));
    let file = OpenOptions::new()
        .append(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                Error::AlreadyExists { path: path.clone() }
            } else {
                Error::Io {
                    op: "create",
                    path: path.clone(),
                    source: e,
                }
            }
        })?;
    let mut w = CdxWriter::new(file, generation, DEFAULT_MAX_LINE);
    w.write_pending().map_err(|source| Error::CdxWrite {
        path: path.clone(),
        source,
    })?;
    w.get_ref().sync_data().map_err(|e| Error::Io {
        op: "fsync",
        path,
        source: e,
    })?;
    masstuffy_durable::sync_dir(dir)?;
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    #[test]
    fn names() {
        let n = segment_naming(7, &Codec::None).unwrap();
        assert_eq!((n.stem(), n.ext()), ("seg.7", "warc"));
        let z = Codec::Zstd {
            dict_id: NonZeroU32::MIN,
            level: 1,
        };
        let n = segment_naming(7, &z).unwrap();
        assert_eq!((n.stem(), n.ext()), ("seg.7", "warc.zst"));
        assert_eq!(cdx_name(7), "index.7.cdx");
    }
}
