//! An open collection and its creation.

use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;

use masstuffy_cdx::Entry;
use masstuffy_seglog::Appender;
use masstuffy_warc::{BuildError, Record, RecordBuilder};

use crate::SOFTWARE;
use crate::context::{GenContext, Options};
use crate::entries::Entries;
use crate::error::{Error, PrepareError};
use crate::id::CollectionId;
use crate::layout::{MANIFEST_NAME, cdx_name, create_cdx, segment_naming};
use crate::manifest::{Codec, Generation, Manifest};
use crate::prepare::Preparer;
use crate::rebuild::{OldGeneration, RebuildOptions, RebuildReport, Rebuilder};
use crate::recover::{self, RecoveryOptions, RecoveryReport};
use crate::resolve::DictResolver;
use crate::snapshot::Snapshot;
use crate::writer::{ScanState, Writer};

/// Parameters of a new collection.
#[derive(Debug, Clone, Copy)]
pub struct CreateSpec<'a> {
    /// The collection uuid.
    pub uuid: CollectionId,
    /// The slug (1..=256 bytes, no control character).
    pub slug: &'a str,
    /// Compression policy of generation 1.
    pub codec: Codec,
    /// Segment size cap.
    pub split_threshold: NonZeroU64,
}

/// An open collection: the single writer and the current snapshot.
pub struct Collection {
    writer: Writer,
    snapshot: Arc<Snapshot>,
}

impl Collection {
    /// Creates the directory `dir` (its parent must exist, `dir` must not), generation 1 and the
    /// warcinfo record; returns the collection and the warcinfo entry (entry 0 of
    /// `index.1.cdx`).
    ///
    /// Order: segment file, CDX header, warcinfo data and CDX line (all fsynced), then the manifest
    /// by atomic replace (the commit point), then an fsync of the parent directory. `create` never
    /// deletes anything on error: a directory without `manifest.json` is not a collection (`open` returns
    /// `ManifestMissing`; the engine trashes it), and one with `manifest.json` after an `Err` must not be registered.
    ///
    /// # Errors
    ///
    /// `Manifest` (invalid slug/level), `InvalidOptions`, `Resolve`, `DictMismatch`,
    /// `InvalidPath`, `AlreadyExists`, `Io`, `Segment`, `CdxWrite`, `Build`, `Prepare`, `Commit`,
    /// `Durable`.
    pub fn create(
        dir: &Path,
        spec: CreateSpec<'_>,
        opts: &Options,
        resolver: &dyn DictResolver,
    ) -> Result<(Collection, Entry<'static>), Error> {
        let manifest_path = dir.join(MANIFEST_NAME);
        let manifest = Manifest::new(
            spec.uuid,
            spec.slug,
            spec.split_threshold,
            Generation {
                id: 1,
                codec: spec.codec,
            },
            2,
        )
        .map_err(|source| Error::Manifest {
            path: manifest_path.clone(),
            source,
        })?;
        let mut json = manifest.to_json().map_err(|source| Error::Manifest {
            path: manifest_path,
            source,
        })?;
        let ctx = Arc::new(GenContext::new(&manifest, opts, resolver)?);

        let parent = match dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => {
                return Err(Error::InvalidPath {
                    path: dir.to_path_buf(),
                });
            }
        };
        fs::create_dir(dir).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                Error::AlreadyExists {
                    path: dir.to_path_buf(),
                }
            } else {
                Error::Io {
                    op: "mkdir",
                    path: dir.to_path_buf(),
                    source: e,
                }
            }
        })?;
        let appender =
            Appender::create(dir, segment_naming(1, &spec.codec)?, spec.split_threshold)?;
        let cdx = create_cdx(dir, 1)?;
        let cdx_path = dir.join(cdx_name(1));
        let header_len = fs::metadata(&cdx_path)
            .map_err(|source| Error::Io {
                op: "stat",
                path: cdx_path.clone(),
                source,
            })?
            .len();
        let state = ScanState {
            cdx_len: header_len,
            entries: 0,
            seg_ends: Vec::new(),
            valid: true,
        };
        let mut writer = Writer::from_parts(Arc::clone(&ctx), appender, cdx, cdx_path, state);
        let mut preparer = Preparer::new(Arc::clone(&ctx))?;
        let prepared = preparer.prepare(warcinfo_record(&json)?)?;
        let entry = writer
            .commit(std::slice::from_ref(&prepared))?
            .into_iter()
            .next()
            .ok_or(Error::Internal("commit returned no entry"))?;

        json.push(b'\n');
        masstuffy_durable::atomic_write(dir, MANIFEST_NAME, &json)?; // commit point
        masstuffy_durable::sync_dir(parent)?;

        let snapshot = Arc::new(Snapshot::new(Arc::new(manifest), ctx, dir, opts)?);
        Ok((Collection { writer, snapshot }, entry))
    }

    /// Opens the collection in `dir`, recovering it after a crash.
    ///
    /// **Precondition**: the caller guarantees exclusive access to `dir` (no other process, and
    /// in this process no other `Collection`, `Writer`, rebuild or manifest rewrite for this
    /// collection; a previous `Writer` has been dropped). No lock is taken.
    ///
    /// Phase A reads only (plus one directory fsync): manifest, dictionaries, directory
    /// classification, segments, then the CDX header (I4) and lines (from the checkpoint if it is
    /// valid), with the cheap I2 check on every scanned line and a digest check on the last
    /// `verify_last` `Add` entries. Phase B mutates: it truncates a torn CDX tail (or, with
    /// `RepairPolicy::TruncateAtCorruption`, cuts at the first corrupt or I2-violating line),
    /// maintains the checkpoint, sweeps `.tmp-*` files and deletes the files of every
    /// non-active generation. Unknown files are kept and reported. A failed phase A changes
    /// nothing on disk. See `RecoveryReport` for what was done.
    ///
    /// # Errors
    ///
    /// `InvalidOptions`, `ManifestMissing` (an aborted `create`: the engine trashes the
    /// directory), `Manifest`, `Io`, `Resolve`, `DictMismatch`, `Durable`, `Segment` (no segment,
    /// a gap, a bad name), `CdxMissing`, `CdxRead`, `CdxGeneration`, `CdxCorrupt`, `I2Violation`,
    /// `Internal`.
    pub fn open(
        dir: &Path,
        opts: &Options,
        recovery: &RecoveryOptions,
        resolver: &dyn DictResolver,
    ) -> Result<(Collection, RecoveryReport), Error> {
        let (writer, snapshot, report) = recover::open(dir, opts, recovery, resolver)?;
        Ok((
            Collection {
                writer,
                snapshot: Arc::new(snapshot),
            },
            report,
        ))
    }

    /// The manifest (the snapshot's).
    #[must_use]
    pub fn manifest(&self) -> &Arc<Manifest> {
        self.snapshot.manifest()
    }

    /// The current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> &Arc<Snapshot> {
        &self.snapshot
    }

    /// Path of the generation's CDX, for re-reading it with `CdxReader::resume` (secondary-index
    /// catch-up).
    #[must_use]
    pub fn cdx_path(&self) -> &Path {
        self.writer.cdx_path()
    }

    /// Writes the scan checkpoint; see [`Writer::checkpoint`].
    ///
    /// # Errors
    ///
    /// As [`Writer::checkpoint`].
    pub fn checkpoint(&mut self) -> Result<bool, Error> {
        self.writer.checkpoint()
    }

    /// The single writer.
    pub fn writer(&mut self) -> &mut Writer {
        &mut self.writer
    }

    /// A new [`Preparer`] for the active generation.
    ///
    /// # Errors
    ///
    /// `Codec(ContextAlloc)` if zstd cannot allocate a context.
    pub fn preparer(&self) -> Result<Preparer, PrepareError> {
        Preparer::new(Arc::clone(self.snapshot.context()))
    }

    /// Live entries of the active generation up to the durable CDX length (see
    /// [`Snapshot::entries`]).
    ///
    /// # Errors
    ///
    /// As [`Snapshot::entries`].
    pub fn entries(&self) -> Result<Entries, Error> {
        self.snapshot.entries(self.writer.cdx_len())
    }

    /// Prepares and writes tombstones for `targets` (one CDX fsync).
    ///
    /// # Errors
    ///
    /// `Tombstone { index, source }` (nothing written), `Commit`.
    pub fn tombstone(&mut self, targets: &[Entry<'_>]) -> Result<Vec<Entry<'static>>, Error> {
        let mut prepared = Vec::with_capacity(targets.len());
        for (index, t) in targets.iter().enumerate() {
            prepared.push(
                self.snapshot
                    .prepare_tombstone(t)
                    .map_err(|source| Error::Tombstone { index, source })?,
            );
        }
        Ok(self.writer.tombstone(&prepared)?)
    }

    /// Offline rebuild (no concurrent writer: `&mut self`): `start`, one `copy_until`, `finish`;
    /// replaces the writer and the snapshot. Returns the report and the old generation (call
    /// `remove` on it). On `Err`, if the writer is poisoned the collection must be dropped and
    /// reopened.
    ///
    /// # Errors
    ///
    /// As [`Rebuilder::start`], [`Rebuilder::copy_until`] and [`Rebuilder::finish`].
    pub fn rebuild(
        &mut self,
        target: Codec,
        opts: &Options,
        ropts: RebuildOptions,
        resolver: &dyn DictResolver,
    ) -> Result<(RebuildReport, OldGeneration), Error> {
        let mut rb = Rebuilder::start(Arc::clone(&self.snapshot), target, opts, ropts, resolver)?;
        rb.copy_until(self.writer.cdx_len())?;
        let r = rb.finish(&mut self.writer)?;
        self.writer = r.writer;
        self.snapshot = r.snapshot;
        Ok((r.report, r.old))
    }

    /// Splits for the engine: the writer task takes the `Writer`, readers clone the
    /// `Arc<Snapshot>`.
    #[must_use]
    pub fn into_parts(self) -> (Writer, Arc<Snapshot>) {
        (self.writer, self.snapshot)
    }
}

/// The first record of a generation: `warcinfo` holding the manifest (spec 02).
pub(crate) fn warcinfo_record(manifest_json: &[u8]) -> Result<Record, BuildError> {
    let mut b = RecordBuilder::new("warcinfo")?;
    b.fill_missing()?;
    b.header("Content-Type", "application/warc-fields")?;
    let mut body = Vec::with_capacity(manifest_json.len() + 128);
    body.extend_from_slice(b"format: WARC File Format 1.1\r\nsoftware: ");
    body.extend_from_slice(SOFTWARE.as_bytes());
    body.extend_from_slice(b"\r\nmasstuffy-collection-manifest: ");
    body.extend_from_slice(manifest_json);
    body.extend_from_slice(b"\r\n");
    b.build(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{CommitError, ReadError};
    use crate::prepare::Prepared;
    use crate::snapshot::{RawEncoding, ReadCtx};
    use crate::testutil::{
        MapResolver, TestDir, create_none, create_with, create_zstd, id_str, names, record,
        record_of_size, trained_id, uuid, zstd_codec,
    };
    use crate::writer::FailPoint;
    use crate::{DEFAULT_SPLIT_THRESHOLD, PLAIN_FRAME_MIN};
    use masstuffy_cdx::{CdxReader, DEFAULT_MAX_LINE, Kind, Step, Tail, digest};
    use masstuffy_seglog::Error as SegError;
    use masstuffy_zframe::inspect_frame;
    use std::collections::HashSet;
    use std::fs::File;
    use std::io::BufReader;
    use std::os::unix::fs::FileExt;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    type Create = fn(&Path) -> (Collection, Entry<'static>);

    fn creators() -> [(&'static str, Create); 2] {
        [("none", create_none), ("zstd", create_zstd)]
    }

    fn cdx_file(dir: &Path) -> File {
        File::open(dir.join("index.1.cdx")).unwrap()
    }

    /// All entries of `index.1.cdx` and how it ends.
    fn read_cdx(dir: &Path) -> (u32, Vec<Entry<'static>>, Tail) {
        let mut r = CdxReader::open(BufReader::new(cdx_file(dir)), DEFAULT_MAX_LINE).unwrap();
        let mut v = Vec::new();
        loop {
            match r.next_step().unwrap() {
                Step::Entry { entry, .. } => v.push(entry.into_owned()),
                Step::End(t) => return (r.generation(), v, t),
            }
        }
    }

    fn commit(c: &mut Collection, p: &mut Preparer, recs: Vec<Record>) -> Vec<Entry<'static>> {
        let batch: Vec<Prepared> = recs.into_iter().map(|r| p.prepare(r).unwrap()).collect();
        c.writer().commit(&batch).unwrap()
    }

    #[test]
    fn create_layout_none_and_zstd() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (c, _) = create(&dir);
            let ext = if name == "none" { "warc" } else { "warc.zst" };
            assert_eq!(
                names(&dir),
                ["index.1.cdx", "manifest.json", &format!("seg.1.1.{ext}")]
            );
            let m = Manifest::load(&dir).unwrap();
            assert_eq!(&m, &**c.manifest());
            assert_eq!(m.next_gen_id(), 2);
            assert_eq!(m.active().id, 1);
        }
    }

    #[test]
    fn warcinfo_is_entry_0() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (c, wi) = create(&dir);
            let (gen_, entries, tail) = read_cdx(&dir);
            assert_eq!(gen_, 1);
            assert_eq!(entries, std::slice::from_ref(&wi));
            assert!(matches!(tail, Tail::Clean { .. }));
            assert_eq!((wi.kind, wi.segment, wi.offset), (Kind::Add, 1, 0));
            assert_eq!(&*wi.warc_type, "warcinfo");

            let mut ctx = ReadCtx::new();
            let mut buf = Vec::new();
            let rec = c.snapshot().read(&wi, &mut ctx, &mut buf).unwrap();
            assert_eq!(rec.header("Content-Type"), Some("application/warc-fields"));
            let body = std::str::from_utf8(rec.body()).unwrap();
            let lines: Vec<&str> = body.split("\r\n").collect();
            assert_eq!(lines[0], "format: WARC File Format 1.1");
            assert_eq!(lines[1], format!("software: {}", crate::SOFTWARE));
            let json = lines[2]
                .strip_prefix("masstuffy-collection-manifest: ")
                .unwrap();
            let on_disk = fs::read(dir.join("manifest.json")).unwrap();
            assert_eq!([json.as_bytes(), b"\n"].concat(), on_disk);
            assert_eq!(
                &Manifest::from_json(json.as_bytes()).unwrap(),
                &**c.manifest()
            );
            if name == "zstd" {
                let seg = fs::read(dir.join("seg.1.1.warc.zst")).unwrap();
                assert_eq!(seg[..4], [0x28, 0xb5, 0x2f, 0xfd]);
                assert_eq!(inspect_frame(&seg).unwrap().dict_id, trained_id());
            }
        }
    }

    #[test]
    fn create_existing_dir_fails() {
        let d = TestDir::new();
        let dir = d.sub();
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("keep"), b"x").unwrap();
        let spec = CreateSpec {
            uuid: uuid(0),
            slug: "t",
            codec: Codec::None,
            split_threshold: DEFAULT_SPLIT_THRESHOLD,
        };
        let r = Collection::create(&dir, spec, &Options::default(), &MapResolver::standard());
        assert!(matches!(r, Err(Error::AlreadyExists { .. })));
        assert_eq!(names(&dir), ["keep"]);
        let missing = d.path().join("no/such/coll");
        let r = Collection::create(
            &missing,
            spec,
            &Options::default(),
            &MapResolver::standard(),
        );
        assert!(matches!(r, Err(Error::Io { op: "mkdir", .. })));
    }

    #[test]
    fn create_bad_dict_leaves_nothing() {
        let d = TestDir::new();
        let dir = d.sub();
        let spec = CreateSpec {
            uuid: uuid(0),
            slug: "t",
            codec: Codec::Zstd {
                dict_id: std::num::NonZeroU32::new(12345).unwrap(),
                level: 1,
            },
            split_threshold: DEFAULT_SPLIT_THRESHOLD,
        };
        let r = Collection::create(&dir, spec, &Options::default(), &MapResolver::standard());
        assert!(matches!(r, Err(Error::Resolve(_))));
        assert!(!dir.exists());
    }

    fn mixed(i: usize, zstd: bool) -> Vec<Record> {
        let mut v = vec![record(i, 0), record(i + 1, 100), record(i + 2, 10 * 1024)];
        if zstd {
            let min = usize::try_from(PLAIN_FRAME_MIN).unwrap();
            v.push(record_of_size(i + 3, min + 10));
        }
        v
    }

    #[test]
    fn read_back_equals_input() {
        for (name, create) in creators() {
            let zstd = name == "zstd";
            let d = TestDir::new();
            let (mut c, _) = create(&d.sub());
            let mut p = c.preparer().unwrap();
            let snap = Arc::clone(c.snapshot());
            let mut ctx = ReadCtx::new();
            let (mut buf, mut raw) = (Vec::new(), Vec::new());
            for round in 0..3 {
                let recs = mixed(round * 10 + 1, zstd);
                let inputs: Vec<Vec<u8>> = recs.iter().map(|r| r.as_bytes().to_vec()).collect();
                let entries = commit(&mut c, &mut p, recs);
                for (e, input) in entries.iter().zip(&inputs) {
                    let rec = snap.read(e, &mut ctx, &mut buf).unwrap();
                    assert_eq!(rec.as_bytes(), &input[..]);
                    let enc = snap.read_raw(e, &mut raw).unwrap();
                    assert_eq!(digest(&raw), e.digest);
                    if zstd {
                        let big = input.len() > usize::try_from(PLAIN_FRAME_MIN).unwrap();
                        let want = if big { 0 } else { trained_id() };
                        assert_eq!(enc, RawEncoding::Zstd { dict_id: want });
                    } else {
                        assert_eq!(enc, RawEncoding::Warc);
                        assert_eq!(raw, *input);
                    }
                }
            }
        }
    }

    #[test]
    fn batch_contiguous() {
        let d = TestDir::new();
        let (mut c, wi) = create_none(&d.sub());
        let mut p = c.preparer().unwrap();
        let mut prev_end = wi.end().unwrap();
        let mut next = 1;
        for n in [1usize, 2, 17] {
            let recs = (0..n).map(|k| record(next + k, 50 + k)).collect();
            next += n;
            let es = commit(&mut c, &mut p, recs);
            assert_eq!(es.len(), n);
            assert_eq!(es[0].offset, prev_end);
            for w in es.windows(2) {
                assert_eq!(w[0].segment, w[1].segment);
                assert_eq!(w[1].offset, w[0].offset + w[0].raw_size);
            }
            prev_end = es[n - 1].end().unwrap();
        }
    }

    #[test]
    fn rollover_never_splits_batch() {
        let d = TestDir::new();
        let dir = d.sub();
        let typical = u64::try_from(record(0, 300).as_bytes().len()).unwrap();
        let threshold = NonZeroU64::new(3 * typical).unwrap();
        let (mut c, wi) = create_with(&dir, Codec::None, threshold, &Options::default());
        let mut p = c.preparer().unwrap();
        let mut batches = vec![vec![wi]];
        let mut next = 1;
        for b in 0..20 {
            let n = 1 + b % 3;
            let recs = (0..n).map(|k| record(next + k, 300)).collect();
            next += n;
            batches.push(commit(&mut c, &mut p, recs));
        }
        let mut per_seg: std::collections::BTreeMap<u32, usize> = Default::default();
        for b in &batches {
            let seg = b[0].segment;
            assert!(b.iter().all(|e| e.segment == seg));
            *per_seg.entry(seg).or_default() += 1;
        }
        let max = *per_seg.keys().max().unwrap();
        assert!(max > 1);
        for (&seg, &count) in &per_seg {
            assert!(per_seg.contains_key(&seg));
            let len = fs::metadata(dir.join(format!("seg.1.{seg}.warc")))
                .unwrap()
                .len();
            if count > 1 {
                assert!(len <= threshold.get(), "segment {seg}: {len}");
            }
        }
        assert_eq!(per_seg.len(), usize::try_from(max).unwrap());
        let snap = c.snapshot();
        let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
        for e in batches.iter().flatten() {
            snap.read(e, &mut ctx, &mut buf).unwrap();
        }
    }

    #[test]
    fn cdx_matches_commits() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, wi) = create_zstd(&dir);
        let mut p = c.preparer().unwrap();
        let mut all = vec![wi];
        for k in 0..4 {
            all.extend(commit(&mut c, &mut p, mixed(k * 10 + 1, false)));
        }
        let (_, entries, tail) = read_cdx(&dir);
        assert_eq!(entries, all);
        assert!(matches!(tail, Tail::Clean { .. }));
    }

    #[test]
    fn commit_empty_batch() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = create_none(&dir);
        let sizes = |dir: &Path| {
            (
                fs::metadata(dir.join("index.1.cdx")).unwrap().len(),
                fs::metadata(dir.join("seg.1.1.warc")).unwrap().len(),
            )
        };
        let before = sizes(&dir);
        assert!(c.writer().commit(&[]).unwrap().is_empty());
        assert_eq!(sizes(&dir), before);
    }

    #[test]
    fn commit_wrong_generation() {
        let d = TestDir::new();
        let (mut c1, _) = create_none(&d.sub());
        let other_dir = d.path().join("other");
        let (c2, _) = Collection::create(
            &other_dir,
            CreateSpec {
                uuid: uuid(1),
                slug: "o",
                codec: Codec::None,
                split_threshold: DEFAULT_SPLIT_THRESHOLD,
            },
            &Options::default(),
            &MapResolver::standard(),
        )
        .unwrap();
        let foreign = c2.preparer().unwrap().prepare(record(1, 10)).unwrap();
        let mut p = c1.preparer().unwrap();
        let ok = p.prepare(record(2, 10)).unwrap();
        let r = c1.writer().commit(&[ok, foreign]);
        assert!(matches!(
            r,
            Err(CommitError::WrongGeneration { index: 1, .. })
        ));
        assert!(!c1.writer().is_poisoned());
        let ok = p.prepare(record(3, 10)).unwrap();
        assert_eq!(c1.writer().commit(&[ok]).unwrap().len(), 1);
    }

    #[test]
    fn commit_fail_points() {
        for point in [FailPoint::BeforeCdxWrite, FailPoint::BeforeCdxSync] {
            let d = TestDir::new();
            let dir = d.sub();
            let (mut c, wi) = create_zstd(&dir);
            let mut p = c.preparer().unwrap();
            let good = commit(&mut c, &mut p, vec![record(1, 20)]);
            let cdx_len = fs::metadata(dir.join("index.1.cdx")).unwrap().len();
            c.writer().fail = Some(point);
            let b = vec![p.prepare(record(2, 20)).unwrap()];
            let err = c.writer().commit(&b).unwrap_err();
            match point {
                FailPoint::BeforeCdxWrite => {
                    assert!(matches!(err, CommitError::CdxWrite { .. }));
                    assert_eq!(
                        fs::metadata(dir.join("index.1.cdx")).unwrap().len(),
                        cdx_len
                    );
                }
                FailPoint::BeforeCdxSync => assert!(matches!(err, CommitError::CdxSync { .. })),
            }
            assert!(c.writer().is_poisoned());
            assert!(matches!(c.writer().commit(&b), Err(CommitError::Poisoned)));
            let snap = c.snapshot();
            let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
            for e in good.iter().chain([&wi]) {
                snap.read(e, &mut ctx, &mut buf).unwrap();
            }
        }
    }

    #[test]
    fn read_rejects_bad_entries() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (mut c, _) = create(&dir);
            let mut p = c.preparer().unwrap();
            let es = commit(&mut c, &mut p, vec![record(1, 100), record(2, 100)]);
            let snap = Arc::clone(c.snapshot());
            let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
            let mut read =
                |e: &Entry<'_>| snap.read(e, &mut ctx, &mut buf).map(|r| r.as_bytes().len());

            let mut e = es[0].clone();
            e.kind = Kind::Tombstone;
            assert!(matches!(read(&e), Err(ReadError::Tombstone)));
            for seg in [0, 99] {
                let mut e = es[0].clone();
                e.segment = seg;
                assert!(matches!(read(&e), Err(ReadError::SegmentMissing { .. })));
            }
            let mut e = es[0].clone();
            e.offset = 1 << 40;
            assert!(matches!(
                read(&e),
                Err(ReadError::Segment(SegError::OutOfBounds { .. }))
            ));
            let mut e = es[0].clone();
            e.raw_size = 4 << 30;
            assert!(matches!(
                read(&e),
                Err(ReadError::Segment(SegError::ReadTooLarge { .. }))
            ));
            let mut e = es[0].clone();
            e.digest ^= 1;
            assert!(matches!(read(&e), Err(ReadError::DigestMismatch { .. })));
            let mut e = es[1].clone();
            e.record_id = es[0].record_id.clone();
            assert!(matches!(
                read(&e),
                Err(ReadError::EntryMismatch("record_id"))
            ));
            let mut e = es[0].clone();
            e.warc_type = "resource".into();
            assert!(matches!(
                read(&e),
                Err(ReadError::EntryMismatch("warc_type"))
            ));
            assert!(read(&es[0]).is_ok());

            if name == "zstd" {
                // One entry covering two frames, digest fixed up.
                let file = fs::read(dir.join("seg.1.1.warc.zst")).unwrap();
                let start = usize::try_from(es[0].offset).unwrap();
                let end = usize::try_from(es[1].end().unwrap()).unwrap();
                let mut e = es[0].clone();
                e.raw_size = es[0].raw_size + es[1].raw_size;
                e.digest = digest(&file[start..end]);
                assert!(matches!(
                    read(&e),
                    Err(ReadError::Frame(_) | ReadError::Codec(_))
                ));
            }
        }
    }

    #[test]
    fn read_corrupted_bytes() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (mut c, _) = create(&dir);
            let mut p = c.preparer().unwrap();
            let es = commit(&mut c, &mut p, vec![record(1, 400)]);
            let path = dir.join(if name == "none" {
                "seg.1.1.warc"
            } else {
                "seg.1.1.warc.zst"
            });
            let f = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            let at = es[0].offset + es[0].raw_size / 2;
            let mut b = [0u8; 1];
            f.read_exact_at(&mut b, at).unwrap();
            f.write_all_at(&[b[0] ^ 0xff], at).unwrap();
            let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
            let r = c.snapshot().read(&es[0], &mut ctx, &mut buf);
            assert!(matches!(r, Err(ReadError::DigestMismatch { .. })), "{name}");
        }
    }

    #[test]
    fn concurrent_readers_while_committing() {
        let d = TestDir::new();
        let dir = d.sub();
        let (c, wi) = create_zstd(&dir);
        let (mut writer, snap) = c.into_parts();
        let published: RwLock<Vec<(Entry<'static>, Vec<u8>)>> = RwLock::new(Vec::new());
        let done = AtomicBool::new(false);
        let size = |i: usize| 50 + (i * 7919) % 20_000;

        // Phase 1: one writer thread, four readers.
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut p = Preparer::new(Arc::clone(writer.context())).unwrap();
                let mut next = 1;
                for b in 0..60 {
                    let n = 1 + b % 5;
                    let mut batch = Vec::new();
                    let mut expected = Vec::new();
                    for k in 0..n {
                        let r = record(next + k, size(next + k));
                        expected.push(r.as_bytes().to_vec());
                        batch.push(p.prepare(r).unwrap());
                    }
                    next += n;
                    let es = writer.commit(&batch).unwrap();
                    published
                        .write()
                        .unwrap()
                        .extend(es.into_iter().zip(expected));
                }
                done.store(true, Ordering::Release);
            });
            for t in 0..4usize {
                let (snap, published, done) = (&snap, &published, &done);
                s.spawn(move || {
                    let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
                    let mut x = t + 1;
                    let mut check = |i: usize| {
                        let (e, want) = {
                            let g = published.read().unwrap();
                            g[i].clone()
                        };
                        let rec = snap.read(&e, &mut ctx, &mut buf).unwrap();
                        assert_eq!(rec.as_bytes(), &want[..]);
                    };
                    loop {
                        let finished = done.load(Ordering::Acquire);
                        let n = published.read().unwrap().len();
                        if n > 0 {
                            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                            check((x >> 8) % n);
                        }
                        if finished {
                            for i in 0..n {
                                check(i);
                            }
                            break;
                        }
                        std::thread::yield_now();
                    }
                });
            }
        });

        // Phase 2: four preparer threads feeding the writer, group commit.
        let (tx, rx) = std::sync::mpsc::sync_channel::<(Prepared, Vec<u8>)>(16);
        let mut batches: Vec<Vec<Entry<'static>>> = Vec::new();
        let mut expected_all: Vec<(Entry<'static>, Vec<u8>)> = Vec::new();
        std::thread::scope(|s| {
            for t in 0..4usize {
                let tx = tx.clone();
                let ctx = Arc::clone(writer.context());
                s.spawn(move || {
                    let mut p = Preparer::new(ctx).unwrap();
                    for j in 0..30 {
                        let i = 10_000 + t * 100 + j;
                        let r = record(i, size(i));
                        let want = r.as_bytes().to_vec();
                        tx.send((p.prepare(r).unwrap(), want)).unwrap();
                    }
                });
            }
            drop(tx);
            while let Ok(first) = rx.recv() {
                let mut group = vec![first];
                while group.len() < 32 {
                    match rx.try_recv() {
                        Ok(x) => group.push(x),
                        Err(_) => break,
                    }
                }
                let (batch, wants): (Vec<_>, Vec<_>) = group.into_iter().unzip();
                let es = writer.commit(&batch).unwrap();
                expected_all.extend(es.iter().cloned().zip(wants));
                batches.push(es);
            }
        });

        let (_, cdx_entries, tail) = read_cdx(&dir);
        assert!(matches!(tail, Tail::Clean { .. }));
        let phase1 = published.into_inner().unwrap();
        assert_eq!(cdx_entries.len(), 1 + phase1.len() + 120);
        assert_eq!(cdx_entries[0], wi);
        let ids: HashSet<&str> = cdx_entries.iter().map(|e| &*e.record_id).collect();
        assert_eq!(ids.len(), cdx_entries.len());
        assert!(ids.contains(id_str(10_000).as_str()));
        for b in &batches {
            for w in b.windows(2) {
                assert_eq!(w[0].segment, w[1].segment);
                assert_eq!(w[1].offset, w[0].offset + w[0].raw_size);
            }
        }
        let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
        for (e, want) in phase1.iter().chain(&expected_all) {
            assert!(cdx_entries.contains(e));
            let rec = snap.read(e, &mut ctx, &mut buf).unwrap();
            assert_eq!(rec.as_bytes(), &want[..]);
        }
    }

    #[test]
    fn header_only_mismatch_is_raw_readable_only() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let (mut c, _) = create(&d.sub());
            let mut p = c.preparer().unwrap();
            let rec = record(1, 100);
            let stored = p.prepare(record(1, 100)).unwrap().bytes;
            let mut meta = masstuffy_cdx::ImportMeta {
                warc_type: "response".into(),
                record_id: "urn:uuid:other".into(),
                url: None,
                date: rec.view().date().unwrap(),
                raw_size: u64::try_from(stored.len()).unwrap(),
                digest: Some(digest(&stored)),
            };
            meta.url = Some("https://other.example/".into());
            let pr = p
                .prepare_raw(&meta, stored.clone(), crate::RawCheck::HeaderOnly)
                .unwrap();
            let es = c.writer().commit(&[pr]).unwrap();
            let snap = Arc::clone(c.snapshot());
            let mut raw = Vec::new();
            snap.read_raw(&es[0], &mut raw).unwrap();
            assert_eq!(raw, stored, "{name}");
            let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
            let r = snap.read(&es[0], &mut ctx, &mut buf);
            assert!(
                matches!(r, Err(ReadError::EntryMismatch("record_id"))),
                "{name}"
            );
        }
    }

    #[test]
    fn send_sync_assertions() {
        fn send<T: Send>() {}
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Snapshot>();
        send_sync::<GenContext>();
        send_sync::<Prepared>();
        send::<Writer>();
        send::<Preparer>();
        send::<ReadCtx>();
        send::<Collection>();
        let _ = zstd_codec();
    }
}
