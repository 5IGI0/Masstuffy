//! The scan checkpoint `scan.<gen>.json`: a derived cache that lets `Collection::open` skip the
//! prefix of the CDX it already verified. Deleting it is always safe (the next open scans fully).
//!
//! Version 2 adds the line counters (M27). An older or newer version is ignored (full scan), so
//! upgrade and downgrade are both safe.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::Deserialize;

use crate::counts::CdxCounts;
use crate::error::Error;
use crate::layout::checkpoint_name;
use crate::recover::FullScanReason;

/// Largest accepted checkpoint file.
pub const MAX_CHECKPOINT_BYTES: u64 = 1 << 20;
/// Largest number of `seg_ends` entries of a checkpoint; with more segments none is written.
pub const MAX_CHECKPOINT_SEGMENTS: usize = 65_536;
/// `open` rewrites the checkpoint after a partial scan only if at least this many CDX bytes were
/// scanned past it.
pub const CHECKPOINT_MIN_GROWTH: u64 = 1 << 20;

/// Content of a checkpoint file (version 2). Compact JSON followed by `"\n"`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointV2 {
    pub(crate) version: u32,
    pub(crate) generation: u32,
    /// A line boundary of the CDX, at or after the header.
    pub(crate) cdx_offset: u64,
    /// Number of lines in `[header, cdx_offset)`.
    pub(crate) entries: u64,
    /// `Add` lines in the prefix.
    pub(crate) adds: u64,
    /// Sum of `raw_size` over the `Add` lines of the prefix.
    pub(crate) add_bytes: u64,
    /// `T` lines in the prefix.
    pub(crate) tombstones: u64,
    /// Sum of `raw_size` over the `T` lines of the prefix.
    pub(crate) tombstone_bytes: u64,
    /// Per segment `i + 1`: the maximum `offset + raw_size` of the prefix's lines (0 if none).
    pub(crate) seg_ends: Vec<u64>,
}

impl CheckpointV2 {
    /// The line counters of the prefix.
    pub(crate) fn counts(&self) -> CdxCounts {
        CdxCounts {
            adds: self.adds,
            add_bytes: self.add_bytes,
            tombstones: self.tombstones,
            tombstone_bytes: self.tombstone_bytes,
        }
    }
}

/// Serialises a checkpoint (no clone of `seg_ends`).
pub(crate) fn encode(
    generation: u32,
    cdx_offset: u64,
    entries: u64,
    counts: &CdxCounts,
    seg_ends: &[u64],
) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(192 + seg_ends.len() * 12);
    // Writing to a String cannot fail; the results are ignored on that ground.
    let _ = write!(
        s,
        "{{\"version\":2,\"generation\":{generation},\"cdx_offset\":{cdx_offset},\"entries\":{entries},\"adds\":{},\"add_bytes\":{},\"tombstones\":{},\"tombstone_bytes\":{},\"seg_ends\":[",
        counts.adds, counts.add_bytes, counts.tombstones, counts.tombstone_bytes
    );
    for (i, e) in seg_ends.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{e}");
    }
    s.push_str("]}\n");
    s.into_bytes()
}

/// Writes `scan.<gen>.json` by atomic replace. Returns `Ok(false)` without writing if the encoding
/// would exceed `MAX_CHECKPOINT_BYTES` (the worst case, 65,536 segments of 20 digits, can: such a
/// file would be rejected on read, so it is not written).
pub(crate) fn write(
    dir: &Path,
    generation: u32,
    cdx_offset: u64,
    entries: u64,
    counts: &CdxCounts,
    seg_ends: &[u64],
) -> Result<bool, Error> {
    let bytes = encode(generation, cdx_offset, entries, counts, seg_ends);
    if u64::try_from(bytes.len()).map_or(true, |n| n > MAX_CHECKPOINT_BYTES) {
        return Ok(false);
    }
    masstuffy_durable::atomic_write(dir, &checkpoint_name(generation), &bytes)?;
    Ok(true)
}

/// Reads and validates the checkpoint of `generation`. Never an error: the reason to fall back to
/// a full scan otherwise.
pub(crate) fn load(dir: &Path, generation: u32) -> Result<CheckpointV2, FullScanReason> {
    let file = File::open(dir.join(checkpoint_name(generation))).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            FullScanReason::NoCheckpoint
        } else {
            FullScanReason::Unreadable
        }
    })?;
    let mut buf = Vec::with_capacity(512);
    file.take(MAX_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|_| FullScanReason::Unreadable)?;
    if u64::try_from(buf.len()).map_or(true, |n| n > MAX_CHECKPOINT_BYTES) {
        return Err(FullScanReason::Malformed);
    }
    let ck: CheckpointV2 = serde_json::from_slice(&buf).map_err(|_| FullScanReason::Malformed)?;
    if ck.version != 2 || ck.seg_ends.len() > MAX_CHECKPOINT_SEGMENTS {
        return Err(FullScanReason::Malformed);
    }
    if ck.adds.checked_add(ck.tombstones) != Some(ck.entries) {
        return Err(FullScanReason::Malformed);
    }
    if ck.generation != generation {
        return Err(FullScanReason::Generation {
            found: ck.generation,
        });
    }
    Ok(ck)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestDir;
    use std::fs;

    fn counts() -> CdxCounts {
        CdxCounts {
            adds: 5,
            add_bytes: 1234,
            tombstones: 2,
            tombstone_bytes: 99,
        }
    }

    fn load_text(txt: &str) -> Result<CheckpointV2, FullScanReason> {
        let d = TestDir::new();
        fs::write(d.path().join(checkpoint_name(1)), txt).unwrap();
        load(d.path(), 1)
    }

    #[test]
    fn checkpoint_v2_roundtrip() {
        let d = TestDir::new();
        assert!(write(d.path(), 1, 500, 7, &counts(), &[10, 0, 30]).unwrap());
        let ck = load(d.path(), 1).unwrap();
        assert_eq!(ck.version, 2);
        assert_eq!((ck.generation, ck.cdx_offset, ck.entries), (1, 500, 7));
        assert_eq!(ck.counts(), counts());
        assert_eq!(ck.seg_ends, [10, 0, 30]);
        let good = String::from_utf8(encode(1, 500, 7, &counts(), &[10])).unwrap();
        assert!(load_text(&good).is_ok());
        let bad = [
            good.replace("\"entries\":7", "\"entries\":8"),
            good.replace("\"adds\":5,", ""),
            good.replace("\"seg_ends\"", "\"extra\":0,\"seg_ends\""),
            good.replace("\"version\":2", "\"version\":1"),
            good.replace("\"version\":2", "\"version\":3"),
        ];
        for (i, b) in bad.iter().enumerate() {
            assert_eq!(load_text(b).unwrap_err(), FullScanReason::Malformed, "{i}");
        }
    }
}
