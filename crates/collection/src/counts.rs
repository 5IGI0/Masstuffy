//! Exact counters of a CDX prefix.
use masstuffy_cdx::Kind;
use masstuffy_seglog::Locator;

/// Exact counts of the lines of one generation's CDX prefix `[header, cdx_len)`. Every field is
/// saturating (overflow is unreachable in practice and never panics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CdxCounts {
    /// `Add` lines (the generation's own warcinfo included).
    pub adds: u64,
    /// Sum of `raw_size` over the `Add` lines.
    pub add_bytes: u64,
    /// `T` lines (duplicates included).
    pub tombstones: u64,
    /// Sum of `raw_size` over the `T` lines.
    pub tombstone_bytes: u64,
}

impl CdxCounts {
    /// `adds + tombstones`, `None` on overflow.
    #[must_use]
    pub fn lines(&self) -> Option<u64> {
        self.adds.checked_add(self.tombstones)
    }

    /// Accounts for one line.
    pub(crate) fn count(&mut self, kind: Kind, raw_size: u64) {
        match kind {
            Kind::Add => {
                self.adds = self.adds.saturating_add(1);
                self.add_bytes = self.add_bytes.saturating_add(raw_size);
            }
            Kind::Tombstone => {
                self.tombstones = self.tombstones.saturating_add(1);
                self.tombstone_bytes = self.tombstone_bytes.saturating_add(raw_size);
            }
        }
    }

    /// Accounts for one committed batch of `Add` lines (one per locator).
    pub(crate) fn add_batch(&mut self, locs: &[Locator]) {
        for l in locs {
            self.count(Kind::Add, l.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_unit() {
        let mut c = CdxCounts::default();
        c.count(Kind::Add, 10);
        c.count(Kind::Tombstone, 4);
        c.count(Kind::Tombstone, 4);
        assert_eq!(
            c,
            CdxCounts {
                adds: 1,
                add_bytes: 10,
                tombstones: 2,
                tombstone_bytes: 8
            }
        );
        assert_eq!(c.lines(), Some(3));
        let locs = [
            Locator {
                segment: 1,
                offset: 0,
                len: 5,
            },
            Locator {
                segment: 1,
                offset: 5,
                len: 7,
            },
        ];
        c.add_batch(&locs);
        assert_eq!((c.adds, c.add_bytes), (3, 22));
        let mut m = CdxCounts {
            adds: u64::MAX,
            add_bytes: u64::MAX,
            tombstones: u64::MAX,
            tombstone_bytes: u64::MAX,
        };
        m.count(Kind::Add, 1);
        m.count(Kind::Tombstone, 1);
        assert_eq!(m.adds, u64::MAX);
        assert_eq!(m.add_bytes, u64::MAX);
        assert_eq!(m.tombstones, u64::MAX);
        assert_eq!(m.tombstone_bytes, u64::MAX);
        assert_eq!(m.lines(), None);
    }

    use crate::recover::{FullScanReason, RecoveryOptions, RepairPolicy, ScanMode};
    use crate::testutil::{
        TestDir, brute_counts, cdx_lines, commit_batch, creators, file_len, flip_byte, open_ok,
        open_with, record, seg_bytes, seg1,
    };
    use std::fs;
    use std::path::Path;

    /// Commits 3 batches, tombstones 2 entries, then one of them again; returns the live entries.
    fn populate(c: &mut crate::Collection) {
        let mut p = c.preparer().unwrap();
        let a = commit_batch(c, &mut p, vec![record(1, 100), record(2, 200)]);
        let b = commit_batch(c, &mut p, vec![record(3, 50)]);
        commit_batch(c, &mut p, vec![record(4, 300), record(5, 10)]);
        c.tombstone(&[a[0].clone(), b[0].clone()]).unwrap();
        c.tombstone(&[a[0].clone()]).unwrap();
    }

    fn check(c: &mut crate::Collection, dir: &Path, zstd: bool) {
        assert_eq!(c.writer().counts(), brute_counts(dir, 1));
        assert_eq!(c.writer().segment_bytes(), seg_bytes(dir, 1, zstd));
        assert_eq!(c.writer().cdx_len(), file_len(&dir.join("index.1.cdx")));
    }

    #[test]
    fn writer_counts_match_cdx() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (mut c, wi) = create(&dir);
            let zstd = name == "zstd";
            assert_eq!(c.writer().counts().adds, 1);
            assert_eq!(c.writer().counts().add_bytes, wi.raw_size);
            populate(&mut c);
            assert_eq!(c.writer().counts().tombstones, 3, "{name}");
            assert_eq!(c.writer().counts().adds, 6, "{name}");
            check(&mut c, &dir, zstd);
        }
    }

    #[test]
    fn counts_survive_reopen_partial_and_full() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (mut c, _) = create(&dir);
            let zstd = name == "zstd";
            populate(&mut c);
            assert!(c.checkpoint().unwrap());
            let mut p = c.preparer().unwrap();
            commit_batch(&mut c, &mut p, vec![record(6, 77)]);
            drop((p, c));

            let (mut c, rep) = open_ok(&dir);
            assert!(matches!(rep.scan, ScanMode::Partial { .. }), "{name}");
            check(&mut c, &dir, zstd);
            drop(c);
            let full = RecoveryOptions {
                full_scan: true,
                ..RecoveryOptions::default()
            };
            let (mut c, rep) = open_with(&dir, &full).unwrap();
            assert!(matches!(rep.scan, ScanMode::Full { .. }));
            check(&mut c, &dir, zstd);
            drop(c);
            fs::remove_file(dir.join("scan.1.json")).unwrap();
            let (mut c, rep) = open_ok(&dir);
            assert_eq!(
                rep.scan,
                ScanMode::Full {
                    reason: FullScanReason::NoCheckpoint
                }
            );
            check(&mut c, &dir, zstd);
        }
    }

    #[test]
    fn counts_after_torn_tail() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = creators()[0].1(&dir);
        populate(&mut c);
        drop(c);
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(dir.join("index.1.cdx"))
            .unwrap();
        std::io::Write::write_all(&mut f, b"A 1 0 12 half a li").unwrap();
        drop(f);
        let (mut c, rep) = open_ok(&dir);
        assert!(rep.cdx_torn.is_some());
        check(&mut c, &dir, false);
    }

    #[test]
    fn counts_after_deep_check_repair_cut() {
        for (name, create) in creators() {
            let d = TestDir::new();
            let dir = d.sub();
            let (mut c, _) = create(&dir);
            let zstd = name == "zstd";
            populate(&mut c);
            drop(c);
            let lines = cdx_lines(&dir, 1);
            // Second-to-last Add: corrupt its data; the last Add and the tombstones lie after it
            // in the CDX, so they are all cut.
            let adds: Vec<_> = lines
                .iter()
                .filter(|l| l.2.kind == masstuffy_cdx::Kind::Add)
                .collect();
            let bad = adds[adds.len() - 2];
            flip_byte(&seg1(&dir, zstd, 1), bad.2.offset + bad.2.raw_size / 2);
            let opts = RecoveryOptions {
                repair: RepairPolicy::TruncateAtCorruption,
                ..RecoveryOptions::default()
            };
            let (mut c, rep) = open_with(&dir, &opts).unwrap();
            assert_eq!(rep.repair.as_ref().map(|r| r.at), Some(bad.0), "{name}");
            assert_eq!(c.writer().counts(), brute_counts(&dir, 1), "{name}");
            assert_eq!(c.writer().counts().adds, 4);
            assert_eq!(c.writer().counts().tombstones, 0);
        }
    }

    #[test]
    fn counts_after_cut_with_tombstones_before() {
        // Tombstones come first, then two Adds; the cut removes only the last Add.
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = creators()[0].1(&dir);
        let mut p = c.preparer().unwrap();
        let a = commit_batch(&mut c, &mut p, vec![record(1, 100), record(2, 100)]);
        c.tombstone(&[a[0].clone()]).unwrap();
        commit_batch(&mut c, &mut p, vec![record(3, 100)]);
        commit_batch(&mut c, &mut p, vec![record(4, 100)]);
        drop((p, c));
        let lines = cdx_lines(&dir, 1);
        let last = &lines[lines.len() - 1];
        flip_byte(&seg1(&dir, false, 1), last.2.offset + last.2.raw_size / 2);
        let opts = RecoveryOptions {
            repair: RepairPolicy::TruncateAtCorruption,
            ..RecoveryOptions::default()
        };
        let (mut c, rep) = open_with(&dir, &opts).unwrap();
        assert_eq!(rep.repair.as_ref().map(|r| r.at), Some(last.0));
        assert_eq!(c.writer().counts(), brute_counts(&dir, 1));
        assert_eq!(c.writer().counts().tombstones, 1);
        assert_eq!(c.writer().counts().adds, 4);
    }

    #[test]
    fn v1_checkpoint_upgrades() {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, _) = creators()[0].1(&dir);
        populate(&mut c);
        drop(c);
        let end = file_len(&dir.join("index.1.cdx"));
        let n = cdx_lines(&dir, 1).len();
        let seg = file_len(&seg1(&dir, false, 1));
        let v1 = format!(
            "{{\"version\":1,\"generation\":1,\"cdx_offset\":{end},\"entries\":{n},\"seg_ends\":[{seg}]}}\n"
        );
        fs::write(dir.join("scan.1.json"), v1).unwrap();
        let (mut c, rep) = open_ok(&dir);
        assert_eq!(
            rep.scan,
            ScanMode::Full {
                reason: FullScanReason::Malformed
            }
        );
        assert_eq!(c.writer().counts(), brute_counts(&dir, 1));
        assert!(rep.checkpoint_written.is_some());
        let txt = fs::read_to_string(dir.join("scan.1.json")).unwrap();
        assert!(txt.starts_with("{\"version\":2,"));
        drop(c);
        let (mut c, rep) = open_ok(&dir);
        assert!(matches!(rep.scan, ScanMode::Partial { .. }));
        assert_eq!(c.writer().counts(), brute_counts(&dir, 1));
    }
}
