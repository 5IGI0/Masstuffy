//! One Masstuffy collection on disk: the versioned manifest, generation 1 creation, batch append
//! with the durable "data, fsync, index, fsync" order, and positioned reads.
//!
//! Everything in this crate is **blocking** (file I/O, fsync, zstd, xxh3): async callers run
//! every call in `spawn_blocking`.
//!
//! Layout of a collection directory (see [`Manifest`]):
//!
//! ```text
//! manifest.json            the single commit point, written only by atomic replace
//! seg.<gen>.<N>.warc       segments of generation <gen> (codec none)
//! seg.<gen>.<N>.warc.zst   segments of generation <gen> (codec zstd)
//! index.<gen>.cdx          the CDX of generation <gen>
//! scan.<gen>.json          optional scan checkpoint (derived cache, safe to delete)
//! ```
//!
//! Flow: [`Collection::create`] builds a collection, [`Collection::open`] loads one and recovers it
//! after a crash; a [`Preparer`] (one per worker thread)
//! turns a record into a [`Prepared`] outside any lock; the single [`Writer`] commits batches of
//! them; readers share an `Arc<`[`Snapshot`]`>` and use a per-thread [`ReadCtx`].
//!
//! Recovery: `open` truncates a torn CDX tail, checks I2 (cheaply on every scanned line, deeply on
//! the last few) and I4, scans only the CDX past the checkpoint when there is a valid one, and
//! deletes the files of non-active generations and `.tmp-*` leftovers. It takes no lock: the
//! caller guarantees exclusive access to the directory (see the `recover` module docs, and
//! [`Collection::open`]).
#![forbid(unsafe_code)]

mod checkpoint;
mod collection;
mod context;
mod error;
mod id;
mod layout;
mod manifest;
mod prepare;
mod recover;
mod resolve;
mod snapshot;
#[cfg(test)]
mod testutil;
mod writer;

use std::num::NonZeroU64;

pub use checkpoint::{CHECKPOINT_MIN_GROWTH, MAX_CHECKPOINT_BYTES, MAX_CHECKPOINT_SEGMENTS};
pub use collection::{Collection, CreateSpec};
pub use context::{GenContext, Options};
pub use error::{CommitError, Error, I2Violation, ManifestError, PrepareError, ReadError};
pub use id::CollectionId;
pub use manifest::{Codec, Generation, Manifest};
pub use masstuffy_cdx::{Entry, Kind, LineError};
pub use prepare::{Prepared, Preparer, RawCheck};
pub use recover::{
    DEFAULT_VERIFY_LAST, FullScanReason, MAX_REPORTED_UNKNOWN, MAX_VERIFY_LAST, RecoveryOptions,
    RecoveryReport, Repair, RepairPolicy, RepairReason, ScanMode, TornTail,
};
pub use resolve::{DictResolver, ResolveError};
pub use snapshot::{RawEncoding, ReadCtx, Snapshot};
pub use writer::Writer;

/// Default segment size cap (spec 02): 2^32 - 1 bytes.
pub const DEFAULT_SPLIT_THRESHOLD: NonZeroU64 = match NonZeroU64::new(4_294_967_295) {
    Some(v) => v,
    None => NonZeroU64::MIN,
};

/// Default zstd level (spec 02 `compression_level`, zstd's default).
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// In a Zstd collection, a serialised record strictly larger than this is compressed without
/// dictionary (frame `dict_id` 0); raw frames with `dict_id` 0 must declare a content size strictly
/// larger than this (architecture section 5).
pub const PLAIN_FRAME_MIN: u64 = 1 << 20;

/// Largest accepted manifest file, in bytes.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// Identifies the writer in the warcinfo record (spec 02: `software: masstuffy/<version>`).
pub const SOFTWARE: &str = concat!("masstuffy/", env!("CARGO_PKG_VERSION"));
