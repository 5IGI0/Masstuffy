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
    /// An internal invariant did not hold.
    #[error("internal error: {0}")]
    Internal(&'static str),
}
