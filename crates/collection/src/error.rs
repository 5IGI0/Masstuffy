//! Error types of the crate.

use std::path::PathBuf;

use crate::resolve::ResolveError;

/// Why a manifest is invalid.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// The manifest is larger than the cap.
    #[error("manifest of {len} bytes exceeds the cap of {cap}")]
    TooLarge {
        /// Size found.
        len: u64,
        /// The cap.
        cap: u64,
    },
    /// Not valid JSON for the schema (syntax, unknown or missing field, wrong type, duplicate key).
    #[error("invalid manifest JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The `version` field is not 1.
    #[error("unsupported manifest version {0}")]
    UnsupportedVersion(u32),
    /// The uuid is not canonical lowercase hyphenated.
    #[error("invalid collection uuid")]
    InvalidUuid,
    /// The slug is empty, too long or has a control character (the reason).
    #[error("invalid slug: {0}")]
    InvalidSlug(&'static str),
    /// `active.id` is 0 or `next_gen_id` is not above it.
    #[error("invalid generation ids (active {active}, next {next})")]
    InvalidGeneration {
        /// `active.id`.
        active: u32,
        /// `next_gen_id`.
        next: u32,
    },
    /// The zstd level is outside zstd's range.
    #[error("invalid zstd level {0}")]
    InvalidLevel(i32),
}

/// Why a record cannot be prepared.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PrepareError {
    /// The serialised record is larger than `Options::max_record_bytes`.
    #[error("record of {size} bytes exceeds the cap of {cap}")]
    RecordTooLarge {
        /// Size found.
        size: u64,
        /// The cap.
        cap: u64,
    },
    /// The stored bytes are larger than the stored-size cap.
    #[error("stored payload of {size} bytes exceeds the cap of {cap}")]
    StoredTooLarge {
        /// Size found.
        size: u64,
        /// The cap.
        cap: u64,
    },
    /// The bytes are not exactly one valid WARC record.
    #[error("invalid WARC record: {0}")]
    Record(#[from] masstuffy_warc::ParseError),
    /// A header needed by the index is missing.
    #[error("record lacks header {0}")]
    MissingHeader(&'static str),
    /// The index entry would be invalid or too long.
    #[error("invalid index entry: {0}")]
    Entry(#[from] masstuffy_cdx::EntryError),
    /// zstd compression or decompression failed.
    #[error("codec error: {0}")]
    Codec(#[from] masstuffy_zframe::CodecError),
    /// The payload is not exactly one zstd frame.
    #[error("invalid zstd frame: {0}")]
    Frame(#[from] masstuffy_zframe::FrameError),
    /// The frame names a dictionary other than the collection's.
    #[error("frame dictionary id {frame} is not the collection's {expected}")]
    UnexpectedDictId {
        /// Id in the frame.
        frame: u32,
        /// The collection's id.
        expected: u32,
    },
    /// A frame without dictionary that does not declare a large content size.
    #[error("frame without dictionary not allowed (declared content size {content_size:?})")]
    PlainFrameNotAllowed {
        /// The declared content size, if any.
        content_size: Option<u64>,
    },
    /// `reprepare` was given a record of another collection.
    #[error("record was prepared for another collection")]
    ForeignContext,
    /// The payload is empty.
    #[error("empty payload")]
    EmptyPayload,
    /// The payload length differs from the declared `raw_size`.
    #[error("declared raw_size {declared} but payload has {actual} bytes")]
    SizeMismatch {
        /// Declared size.
        declared: u64,
        /// Actual size.
        actual: u64,
    },
    /// The claimed digest differs from the computed one.
    #[error("claimed digest {claimed:016x} but payload digest is {actual:016x}")]
    DigestMismatch {
        /// Claimed digest.
        claimed: u64,
        /// Computed digest.
        actual: u64,
    },
    /// The envelope metadata disagrees with the record (the field).
    #[error("envelope metadata does not match the record: {0}")]
    MetaMismatch(&'static str),
    /// A frame header declares a content (or window) size above `max_record_bytes`.
    #[error("frame declares a size of {size} bytes above the cap of {cap}")]
    DeclaredTooLarge {
        /// Declared size.
        size: u64,
        /// The cap.
        cap: u64,
    },
}

/// Why an entry cannot be tombstoned.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TombstoneError {
    /// The entry is already a tombstone.
    #[error("entry is a tombstone")]
    NotAnAdd,
    /// The entry is the generation's warcinfo record (segment 1, offset 0).
    #[error("the generation's warcinfo record cannot be deleted")]
    Warcinfo,
    /// The entry is invalid or its tombstone line would exceed the line cap.
    #[error("invalid entry: {0}")]
    Entry(#[from] masstuffy_cdx::EntryError),
    /// The segment does not exist in this generation.
    #[error("segment {segment} is missing")]
    SegmentMissing {
        /// The segment number.
        segment: u32,
    },
    /// `offset + raw_size` passes the end of the segment.
    #[error("segment {segment}: end {end} passes its length {len}")]
    PastEnd {
        /// The segment number.
        segment: u32,
        /// `offset + raw_size`.
        end: u64,
        /// Length of the segment.
        len: u64,
    },
    /// Reading the segment length failed.
    #[error("segment stat failed: {0}")]
    Segment(#[source] masstuffy_seglog::Error),
}

/// Why a batch could not be committed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CommitError {
    /// An earlier commit failed with an I/O error; reopen the collection.
    #[error("writer is poisoned by an earlier error")]
    Poisoned,
    /// An item was prepared for another collection or generation.
    #[error(
        "item {index} was prepared for another collection or generation (expected generation {expected}, got {got})"
    )]
    WrongGeneration {
        /// Index in the batch.
        index: usize,
        /// The writer's generation.
        expected: u32,
        /// The item's generation.
        got: u32,
    },
    /// A tombstone targets bytes that no committed (fsynced) CDX line covers.
    #[error(
        "tombstone {index}: segment {segment} end {end} is beyond the committed end {committed}"
    )]
    TombstoneNotCommitted {
        /// Index in the request.
        index: usize,
        /// The segment.
        segment: u32,
        /// `offset + raw_size` of the target.
        end: u64,
        /// The committed end of that segment known to the writer (or its file length).
        committed: u64,
    },
    /// Appending or syncing the data failed.
    #[error("data append failed: {0}")]
    Data(#[source] masstuffy_seglog::Error),
    /// An index line could not be encoded.
    #[error("index entry error: {0}")]
    Entry(#[source] masstuffy_cdx::EntryError),
    /// Writing the index failed.
    #[error("writing {} failed: {source}", path.display())]
    CdxWrite {
        /// The index file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: masstuffy_cdx::WriteError,
    },
    /// Syncing the index failed.
    #[error("fsync of {} failed: {source}", path.display())]
    CdxSync {
        /// The index file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// Why a record cannot be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadError {
    /// The entry is a tombstone.
    #[error("entry is a tombstone")]
    Tombstone,
    /// The segment file does not exist (possibly a stale snapshot).
    #[error("segment {segment} is missing")]
    SegmentMissing {
        /// The segment number.
        segment: u32,
    },
    /// Reading the segment failed.
    #[error("segment read failed: {0}")]
    Segment(#[source] masstuffy_seglog::Error),
    /// The stored bytes do not match the entry's digest.
    #[error("digest mismatch (expected {expected:016x}, actual {actual:016x})")]
    DigestMismatch {
        /// Digest in the entry.
        expected: u64,
        /// Digest of the bytes read.
        actual: u64,
    },
    /// The stored bytes are not one zstd frame.
    #[error("invalid zstd frame: {0}")]
    Frame(#[from] masstuffy_zframe::FrameError),
    /// Decompression failed.
    #[error("codec error: {0}")]
    Codec(#[from] masstuffy_zframe::CodecError),
    /// The decoded bytes are not one valid WARC record.
    #[error("invalid WARC record: {0}")]
    Record(#[from] masstuffy_warc::ParseError),
    /// The record does not match its index entry (the field).
    #[error("record does not match its index entry: {0}")]
    EntryMismatch(&'static str),
}

/// How a CDX line violates I2 (spec 05 §5.1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum I2Violation {
    /// The segment number is 0 or above the number of segments of the generation.
    #[error("segment {segment} does not exist (generation has {segments})")]
    SegmentMissing {
        /// Segment named by the line.
        segment: u32,
        /// Number of segments of the generation.
        segments: u32,
    },
    /// `offset + raw_size` passes the end of the segment (or overflows).
    #[error("segment {segment}: [{offset}, +{raw_size}) passes its end {segment_len}")]
    PastEnd {
        /// Segment named by the line.
        segment: u32,
        /// Offset in the segment.
        offset: u64,
        /// Stored size.
        raw_size: u64,
        /// Length of the segment file.
        segment_len: u64,
    },
    /// The stored bytes do not have the entry's digest (deep check).
    #[error("segment {segment} offset {offset}: digest {actual:016x}, entry says {expected:016x}")]
    DigestMismatch {
        /// Segment named by the line.
        segment: u32,
        /// Offset in the segment.
        offset: u64,
        /// Stored size.
        raw_size: u64,
        /// Digest in the entry.
        expected: u64,
        /// Digest of the bytes read.
        actual: u64,
    },
}

/// Errors of collection-level operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An option is invalid (the field).
    #[error("invalid option {0}")]
    InvalidOptions(&'static str),
    /// The path has no parent directory.
    #[error("invalid collection path {}", path.display())]
    InvalidPath {
        /// The path.
        path: PathBuf,
    },
    /// The path already exists.
    #[error("{} already exists", path.display())]
    AlreadyExists {
        /// The path.
        path: PathBuf,
    },
    /// The directory has no `manifest.json`.
    #[error("manifest missing: {}", path.display())]
    ManifestMissing {
        /// The manifest path.
        path: PathBuf,
    },
    /// The manifest is invalid.
    #[error("invalid manifest {}: {source}", path.display())]
    Manifest {
        /// The manifest path.
        path: PathBuf,
        /// Why.
        #[source]
        source: ManifestError,
    },
    /// An I/O call failed.
    #[error("{op} failed on {}: {source}", path.display())]
    Io {
        /// The operation.
        op: &'static str,
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A durable filesystem primitive failed.
    #[error(transparent)]
    Durable(#[from] masstuffy_durable::Error),
    /// The segment log failed.
    #[error(transparent)]
    Segment(#[from] masstuffy_seglog::Error),
    /// Writing the index header failed.
    #[error("writing {} failed: {source}", path.display())]
    CdxWrite {
        /// The index file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: masstuffy_cdx::WriteError,
    },
    /// A dictionary could not be resolved.
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    /// The resolver returned the wrong dictionary.
    #[error("resolver returned the wrong dictionary ({what}: expected {expected}, got {got})")]
    DictMismatch {
        /// What differs.
        what: &'static str,
        /// Expected value.
        expected: i64,
        /// Value found.
        got: i64,
    },
    /// The warcinfo record could not be built.
    #[error("cannot build the warcinfo record: {0}")]
    Build(#[from] masstuffy_warc::BuildError),
    /// Preparing a record failed.
    #[error(transparent)]
    Prepare(#[from] PrepareError),
    /// Committing a batch failed.
    #[error(transparent)]
    Commit(#[from] CommitError),
    /// The active generation's CDX does not exist.
    #[error("index missing: {}", path.display())]
    CdxMissing {
        /// The index path.
        path: PathBuf,
    },
    /// Reading the CDX failed (I/O, missing or bad header, offset overflow).
    #[error("reading {} failed: {source}", path.display())]
    CdxRead {
        /// The index path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: masstuffy_cdx::ReadError,
    },
    /// The CDX header names another generation than the manifest (I4).
    #[error("{} is for generation {found}, manifest says {expected}", path.display())]
    CdxGeneration {
        /// The index path.
        path: PathBuf,
        /// Generation in the manifest.
        expected: u32,
        /// Generation in the header.
        found: u32,
    },
    /// Corruption that is not a torn tail, at byte `at`. Repairable with
    /// `RepairPolicy::TruncateAtCorruption`.
    #[error("corrupt index {} at byte {at} of {end}: {source}", path.display())]
    CdxCorrupt {
        /// The index path.
        path: PathBuf,
        /// Offset of the first bad line.
        at: u64,
        /// File length.
        end: u64,
        /// What is wrong with the line.
        #[source]
        source: masstuffy_cdx::LineError,
    },
    /// The CDX line at byte `at` violates I2. Repairable with
    /// `RepairPolicy::TruncateAtCorruption`.
    #[error("index {} line at byte {at} violates I2: {violation}", path.display())]
    I2Violation {
        /// The index path.
        path: PathBuf,
        /// Offset of the offending line.
        at: u64,
        /// How it violates I2.
        #[source]
        violation: I2Violation,
    },
    /// A CDX byte range is invalid: bounds below the header, `end` before `from`, or `from` not
    /// right after a `\n`.
    #[error("invalid index range [{from}, {end}) of {}", path.display())]
    CdxRange {
        /// The index path.
        path: PathBuf,
        /// Start of the range.
        from: u64,
        /// End of the range.
        end: u64,
    },
    /// A CDX range ends inside a line, or the file is shorter than the range.
    #[error("index {} is truncated at byte {at} (range end {end})", path.display())]
    CdxTruncated {
        /// The index path.
        path: PathBuf,
        /// Where the data ran out.
        at: u64,
        /// End of the requested range.
        end: u64,
    },
    /// An entry of `Collection::tombstone` cannot be tombstoned.
    #[error("tombstone target {index}: {source}")]
    Tombstone {
        /// Index in the request.
        index: usize,
        /// Why.
        #[source]
        source: TombstoneError,
    },
    /// The rebuild source (the named thing) is no longer the collection's active state.
    #[error("rebuild source is stale ({0} changed)")]
    StaleSource(&'static str),
    /// An earlier error stopped this rebuild; abort it.
    #[error("rebuild is poisoned by an earlier error")]
    RebuildPoisoned,
    /// The rebuild's cancel flag was set (`Rebuilder::set_cancel`); abort it.
    #[error("rebuild cancelled")]
    RebuildCancelled,
    /// A tombstone was written after the rebuild copied its first range.
    #[error("tombstone at index byte {cdx_offset} written during a rebuild")]
    TombstoneDuringRebuild {
        /// Offset of the `T` line in the old CDX.
        cdx_offset: u64,
    },
    /// The `Add` lines of the CDX are not in strictly increasing locator order.
    #[error("index line at byte {cdx_offset} is out of locator order")]
    LocatorOrder {
        /// Offset of the offending line in the old CDX.
        cdx_offset: u64,
    },
    /// Reading a record of the old generation failed during a rebuild.
    #[error("rebuild: reading segment {segment} offset {offset} failed: {source}")]
    RebuildRead {
        /// Segment of the record.
        segment: u32,
        /// Offset of the record.
        offset: u64,
        /// Why.
        #[source]
        source: ReadError,
    },
    /// Re-encoding a record failed during a rebuild.
    #[error("rebuild: re-encoding segment {segment} offset {offset} failed: {source}")]
    RebuildPrepare {
        /// Segment of the record.
        segment: u32,
        /// Offset of the record.
        offset: u64,
        /// Why.
        #[source]
        source: PrepareError,
    },
    /// A rebuild worker thread panicked; the rebuild stopped.
    #[error("a rebuild worker thread panicked")]
    WorkerPanicked,
    /// An internal invariant did not hold.
    #[error("internal error: {0}")]
    Internal(&'static str),
}
