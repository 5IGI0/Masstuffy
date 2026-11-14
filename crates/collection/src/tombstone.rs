//! Validation of logical record deletions.

use std::sync::Arc;

use masstuffy_cdx::{DEFAULT_MAX_LINE, Entry, EntryError, Kind, encode_entry};

use crate::context::GenContext;
use crate::error::TombstoneError;
use crate::snapshot::Snapshot;

/// A tombstone ready for `Writer::tombstone`: the `T` entry (all fields of the deleted `Add`) and
/// the generation it was validated against. Built only by `Snapshot::prepare_tombstone`.
#[derive(Debug, Clone)]
pub struct PreparedTombstone {
    pub(crate) ctx: Arc<GenContext>,
    pub(crate) entry: Entry<'static>,
}

impl PreparedTombstone {
    /// The `T` entry that will be written.
    #[must_use]
    pub fn entry(&self) -> &Entry<'static> {
        &self.entry
    }

    /// The generation context it was validated against.
    #[must_use]
    pub fn context(&self) -> &Arc<GenContext> {
        &self.ctx
    }
}

impl Snapshot {
    /// Validates the deletion of `entry` (an `Add` entry of this snapshot's generation). Cheap:
    /// no data is read and no index is consulted (one `fstat` on a cached handle); safe to call
    /// from any thread, before queueing to the writer.
    ///
    /// Checks: kind `Add`, `Entry::validate`, the `T` line fits the line cap, the entry is not the
    /// generation's warcinfo (segment 1, offset 0), the segment exists and `offset + raw_size` is
    /// within its length (so the `T` line satisfies the cheap I2 check of `Collection::open`). It
    /// `Writer::tombstone` additionally requires the target to be covered by a committed line.
    /// It does not check that an `Add` line with that locator exists: a `T` line for an unknown
    /// locator removes nothing. A stale snapshot is caught by `Writer::tombstone`.
    ///
    /// # Errors
    ///
    /// See [`TombstoneError`].
    pub fn prepare_tombstone(
        &self,
        entry: &Entry<'_>,
    ) -> Result<PreparedTombstone, TombstoneError> {
        if entry.kind != Kind::Add {
            return Err(TombstoneError::NotAnAdd);
        }
        entry.validate()?;
        if entry.segment == 1 && entry.offset == 0 {
            return Err(TombstoneError::Warcinfo);
        }
        let t = Entry {
            kind: Kind::Tombstone,
            ..entry.clone()
        }
        .into_owned();
        let mut line = Vec::with_capacity(512);
        encode_entry(&t, DEFAULT_MAX_LINE, &mut line)?;
        let len = self.reader().len_of(entry.segment).map_err(|e| match e {
            masstuffy_seglog::Error::SegmentNotFound { segment } => {
                TombstoneError::SegmentMissing { segment }
            }
            e => TombstoneError::Segment(e),
        })?;
        let end = entry.end().ok_or(EntryError::LocatorOverflow)?;
        if end > len {
            return Err(TombstoneError::PastEnd {
                segment: entry.segment,
                end,
                len,
            });
        }
        Ok(PreparedTombstone {
            ctx: Arc::clone(self.context()),
            entry: t,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::Collection;
    use crate::error::{CommitError, Error};
    use crate::recover::RecoveryOptions;
    use crate::testutil::{
        TestDir, cdx_lines, commit_batch, create_none, creators, file_len, id_str, live, open_ok,
        open_with, read_bytes, record, seg1,
    };
    use crate::writer::FailPoint;
    use std::path::Path;

    /// Commits records 1..=n in one batch and returns their entries.
    fn setup(
        create: crate::testutil::Create,
        dir: &Path,
        n: usize,
    ) -> (Collection, Vec<Entry<'static>>) {
        let (mut c, _) = create(dir);
        let mut p = c.preparer().unwrap();
        let es = commit_batch(&mut c, &mut p, (1..=n).map(|i| record(i, 60)).collect());
        (c, es)
    }

    fn t_line_len(e: &Entry<'_>) -> u64 {
        let t = Entry {
            kind: Kind::Tombstone,
            ..e.clone()
        };
        let mut v = Vec::new();
        encode_entry(&t, DEFAULT_MAX_LINE, &mut v).unwrap();
        u64::try_from(v.len()).unwrap()
    }

    #[test]
    fn tombstone_durable() {
        for (_, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (mut c, es) = setup(create, &dir, 3);
            let before = c.writer().cdx_len();
            let out = c.tombstone(std::slice::from_ref(&es[1])).unwrap();
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].kind, Kind::Tombstone);
            assert_eq!(
                out[0],
                Entry {
                    kind: Kind::Tombstone,
                    ..es[1].clone()
                }
            );
            assert_eq!(c.writer().cdx_len(), before + t_line_len(&es[1]));
            assert_eq!(c.writer().cdx_len(), file_len(&dir.join("index.1.cdx")));
            drop(c);
            let (c, rep) = open_ok(&dir);
            assert_eq!(rep.entries, 5);
            let got = live(&c);
            assert!(!got.contains(&es[1]));
            assert_eq!(got.len(), 3);
            // Logical deletion only: the bytes are untouched.
            read_bytes(c.snapshot(), &es[1]);
        }
    }

    #[test]
    fn tombstone_rejects_uncommitted_bytes() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, es) = setup(create_none, &dir, 2);
        // Orphan bytes after the last committed record, never indexed.
        let seg = seg1(&dir, false, 1);
        let committed = file_len(&seg);
        let mut f = std::fs::OpenOptions::new().append(true).open(&seg).unwrap();
        std::io::Write::write_all(&mut f, &[b'x'; 100]).unwrap();
        let forged = Entry {
            offset: committed,
            raw_size: 100,
            ..es[0].clone()
        };
        let t = c.snapshot().prepare_tombstone(&forged).unwrap();
        let before = c.writer().cdx_len();
        assert!(matches!(
            c.writer().tombstone(&[t]),
            Err(CommitError::TombstoneNotCommitted { index: 0, .. })
        ));
        assert!(!c.writer().is_poisoned());
        assert_eq!(c.writer().cdx_len(), before);
        c.tombstone(std::slice::from_ref(&es[0])).unwrap();
    }

    #[test]
    fn tombstone_idempotent() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, es) = setup(create_none, &dir, 3);
        c.tombstone(std::slice::from_ref(&es[0])).unwrap();
        let after_first = live(&c);
        c.tombstone(std::slice::from_ref(&es[0])).unwrap();
        assert_eq!(live(&c), after_first);
        let t = cdx_lines(&dir, 1)
            .iter()
            .filter(|l| l.2.kind == Kind::Tombstone)
            .count();
        assert_eq!(t, 2);
        assert!(c.tombstone(&[]).unwrap().is_empty());
    }

    #[test]
    fn tombstone_rejects() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (mut c, es) = setup(create, &dir, 2);
            let wi = live(&c).remove(0);
            let snap = Arc::clone(c.snapshot());
            let seg_len = file_len(&seg1(&dir, name == "zstd", 1));
            let t = Entry {
                kind: Kind::Tombstone,
                ..es[0].clone()
            };
            assert!(matches!(
                snap.prepare_tombstone(&t),
                Err(TombstoneError::NotAnAdd)
            ));
            assert!(matches!(
                snap.prepare_tombstone(&wi),
                Err(TombstoneError::Warcinfo)
            ));
            let e2 = Entry {
                segment: 2,
                ..es[0].clone()
            };
            assert!(matches!(
                snap.prepare_tombstone(&e2),
                Err(TombstoneError::SegmentMissing { segment: 2 })
            ));
            let past = Entry {
                offset: seg_len,
                ..es[0].clone()
            };
            assert!(matches!(
                snap.prepare_tombstone(&past),
                Err(TombstoneError::PastEnd { segment: 1, .. })
            ));
            let bad_id = Entry {
                record_id: "has space".into(),
                ..es[0].clone()
            };
            assert!(matches!(
                snap.prepare_tombstone(&bad_id),
                Err(TombstoneError::Entry(_))
            ));
            let zero = Entry {
                raw_size: 0,
                ..es[0].clone()
            };
            assert!(matches!(
                snap.prepare_tombstone(&zero),
                Err(TombstoneError::Entry(
                    masstuffy_cdx::EntryError::ZeroRawSize
                ))
            ));
            let ok = snap.prepare_tombstone(&es[0]).unwrap();
            assert_eq!(ok.entry().kind, Kind::Tombstone);
            assert_eq!(ok.context().generation(), 1);

            let (len, file) = (c.writer().cdx_len(), file_len(&dir.join("index.1.cdx")));
            let r = c.tombstone(&[es[0].clone(), zero.clone()]);
            assert!(matches!(r, Err(Error::Tombstone { index: 1, .. })));
            assert_eq!(c.writer().cdx_len(), len);
            assert_eq!(file_len(&dir.join("index.1.cdx")), file);
        }
    }

    #[test]
    fn tombstone_fail_points() {
        for point in [FailPoint::BeforeCdxWrite, FailPoint::BeforeCdxSync] {
            let d = TestDir::new();
            let dir = d.sub();
            let (mut c, es) = setup(create_none, &dir, 2);
            c.writer().fail = Some(point);
            let r = c.tombstone(std::slice::from_ref(&es[0]));
            match point {
                FailPoint::BeforeCdxWrite => {
                    assert!(matches!(
                        r,
                        Err(Error::Commit(CommitError::CdxWrite { .. }))
                    ));
                }
                FailPoint::BeforeCdxSync => {
                    assert!(matches!(r, Err(Error::Commit(CommitError::CdxSync { .. }))));
                }
            }
            assert!(c.writer().is_poisoned());
            assert!(matches!(
                c.tombstone(std::slice::from_ref(&es[1])),
                Err(Error::Commit(CommitError::Poisoned))
            ));
            drop(c);
            let (c, _) = open_ok(&dir);
            let live_es = live(&c);
            match point {
                // Nothing written.
                FailPoint::BeforeCdxWrite => assert!(live_es.contains(&es[0])),
                // The line reached the page cache: present, the record is not live.
                FailPoint::BeforeCdxSync => assert!(!live_es.contains(&es[0])),
            }
        }
    }

    #[test]
    fn tombstone_wrong_generation() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, es) = setup(create_none, &dir, 2);
        let t = c.snapshot().prepare_tombstone(&es[0]).unwrap();
        c.rebuild(
            crate::testutil::none_codec(),
            &crate::context::Options::default(),
            crate::rebuild::RebuildOptions::default(),
            &crate::testutil::MapResolver::standard(),
        )
        .unwrap();
        assert!(matches!(
            c.writer().tombstone(&[t]),
            Err(CommitError::WrongGeneration {
                expected: 2,
                got: 1,
                ..
            })
        ));
    }

    #[test]
    fn tombstone_line_passes_open_i2() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, es) = setup(create_none, &dir, 3);
        c.tombstone(&[es[2].clone()]).unwrap();
        drop(c);
        let opts = RecoveryOptions {
            full_scan: true,
            ..RecoveryOptions::default()
        };
        let (c, rep) = open_with(&dir, &opts).unwrap();
        assert!(rep.is_clean());
        assert_eq!(rep.entries, 5);
        assert!(!live(&c).iter().any(|e| *e.record_id == id_str(3)));
    }
}
