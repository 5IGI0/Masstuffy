//! File names of a collection directory and creation of a generation's CDX.

use std::ffi::OsStr;
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

/// Name of the scan checkpoint of generation `gen` (a derived cache, see `recover`).
pub(crate) fn checkpoint_name(generation: u32) -> String {
    format!("scan.{generation}.json")
}

/// What a directory entry name of a collection is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileClass {
    /// `manifest.json`.
    Manifest,
    /// `.lock`, `.tmp-*`, `.trash-*`.
    Reserved,
    /// `seg.<g>.<n>.warc` (`zst` false) or `seg.<g>.<n>.warc.zst` (`zst` true).
    Segment { generation: u32, zst: bool },
    /// `index.<g>.cdx`.
    Cdx { generation: u32 },
    /// `scan.<g>.json`.
    Checkpoint { generation: u32 },
    /// Anything else, including non-UTF-8 names.
    Unknown,
}

/// Canonical decimal `u32 >= 1`: 1..=10 ASCII digits, no leading `0`, no sign.
fn parse_canonical_u32(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 10 || b.first() == Some(&b'0') || !b.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    s.parse::<u32>().ok().filter(|&n| n >= 1)
}

/// Classifies a directory entry name (see [`FileClass`]).
pub(crate) fn classify(name: &OsStr) -> FileClass {
    if masstuffy_durable::is_reserved(name) {
        return FileClass::Reserved;
    }
    let Some(s) = name.to_str() else {
        return FileClass::Unknown;
    };
    if s == MANIFEST_NAME {
        return FileClass::Manifest;
    }
    if let Some(rest) = s.strip_prefix("seg.") {
        let (middle, zst) = if let Some(m) = rest.strip_suffix(".warc.zst") {
            (m, true)
        } else if let Some(m) = rest.strip_suffix(".warc") {
            (m, false)
        } else {
            return FileClass::Unknown;
        };
        let parsed = middle
            .split_once('.')
            .and_then(|(g, n)| Some((parse_canonical_u32(g)?, parse_canonical_u32(n)?)));
        return match parsed {
            Some((generation, _)) => FileClass::Segment { generation, zst },
            None => FileClass::Unknown,
        };
    }
    if let Some(g) = s
        .strip_prefix("index.")
        .and_then(|r| r.strip_suffix(".cdx"))
        .and_then(parse_canonical_u32)
    {
        return FileClass::Cdx { generation: g };
    }
    if let Some(g) = s
        .strip_prefix("scan.")
        .and_then(|r| r.strip_suffix(".json"))
        .and_then(parse_canonical_u32)
    {
        return FileClass::Checkpoint { generation: g };
    }
    FileClass::Unknown
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
        assert_eq!(checkpoint_name(7), "scan.7.json");
    }

    #[test]
    fn classify_table() {
        use std::os::unix::ffi::OsStrExt;
        let c = |s: &str| classify(OsStr::new(s));
        let seg = |generation, zst| FileClass::Segment { generation, zst };
        assert_eq!(c("manifest.json"), FileClass::Manifest);
        for r in [".lock", ".tmp-1-2", ".trash-x"] {
            assert_eq!(c(r), FileClass::Reserved, "{r}");
        }
        assert_eq!(c("seg.1.1.warc"), seg(1, false));
        assert_eq!(c("seg.12.3.warc.zst"), seg(12, true));
        assert_eq!(c("seg.4294967295.4294967295.warc"), seg(u32::MAX, false));
        assert_eq!(c("index.7.cdx"), FileClass::Cdx { generation: 7 });
        assert_eq!(c("scan.3.json"), FileClass::Checkpoint { generation: 3 });
        for u in [
            "seg.0.1.warc",
            "seg.1.0.warc",
            "seg.01.1.warc",
            "seg.1.01.warc",
            "seg.4294967296.1.warc",
            "seg.1.warc",
            "seg.1.1.1.warc",
            "seg.1.1.warc.gz",
            "seg.1.1.zst",
            "seg.+1.1.warc",
            "index.cdx",
            "index.0.cdx",
            "index.01.cdx",
            "index.1.cdx.bak",
            "index..cdx",
            "seg..1.warc",
            "manifest.json.tmp",
            "scan.03.json",
            "scan.json",
            "scan.1.json.tmp",
            "x",
        ] {
            assert_eq!(c(u), FileClass::Unknown, "{u}");
        }
        assert_eq!(
            classify(OsStr::from_bytes(b"seg.1.\xff.warc")),
            FileClass::Unknown
        );
    }
}
