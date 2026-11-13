//! The scan checkpoint `scan.<gen>.json`: a derived cache that lets `Collection::open` skip the
//! prefix of the CDX it already verified. Deleting it is always safe (the next open scans fully).

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::Deserialize;

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

/// Content of a checkpoint file (version 1). Compact JSON followed by `"\n"`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointV1 {
    pub(crate) version: u32,
    pub(crate) generation: u32,
    /// A line boundary of the CDX, at or after the header.
    pub(crate) cdx_offset: u64,
    /// Number of lines in `[header, cdx_offset)`.
    pub(crate) entries: u64,
    /// Per segment `i + 1`: the maximum `offset + raw_size` of the prefix's lines (0 if none).
    pub(crate) seg_ends: Vec<u64>,
}

/// Serialises a checkpoint (no clone of `seg_ends`).
pub(crate) fn encode(generation: u32, cdx_offset: u64, entries: u64, seg_ends: &[u64]) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(96 + seg_ends.len() * 12);
    // Writing to a String cannot fail; the results are ignored on that ground.
    let _ = write!(
        s,
        "{{\"version\":1,\"generation\":{generation},\"cdx_offset\":{cdx_offset},\"entries\":{entries},\"seg_ends\":["
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
    seg_ends: &[u64],
) -> Result<bool, Error> {
    let bytes = encode(generation, cdx_offset, entries, seg_ends);
    if u64::try_from(bytes.len()).map_or(true, |n| n > MAX_CHECKPOINT_BYTES) {
        return Ok(false);
    }
    masstuffy_durable::atomic_write(dir, &checkpoint_name(generation), &bytes)?;
    Ok(true)
}

/// Reads and validates the checkpoint of `generation`. Never an error: the reason to fall back to
/// a full scan otherwise.
pub(crate) fn load(dir: &Path, generation: u32) -> Result<CheckpointV1, FullScanReason> {
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
    let ck: CheckpointV1 = serde_json::from_slice(&buf).map_err(|_| FullScanReason::Malformed)?;
    if ck.version != 1 || ck.seg_ends.len() > MAX_CHECKPOINT_SEGMENTS {
        return Err(FullScanReason::Malformed);
    }
    if ck.generation != generation {
        return Err(FullScanReason::Generation {
            found: ck.generation,
        });
    }
    Ok(ck)
}
