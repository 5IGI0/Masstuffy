//! Streaming iteration over the live entries of a CDX prefix.

use masstuffy_cdx::{Entry, Kind};

use crate::error::Error;
use crate::range::LineReader;
use crate::snapshot::Snapshot;

/// Locators `(segment, offset)` tombstoned by the `T` lines of a range, sorted and deduplicated.
pub(crate) fn collect_tombstones(r: &mut LineReader) -> Result<Vec<(u32, u64)>, Error> {
    let mut v = Vec::new();
    while let Some(at) = r.advance()? {
        let e = r.parse_current(at)?;
        if e.kind == Kind::Tombstone {
            v.push((e.segment, e.offset));
        }
    }
    v.sort_unstable();
    v.dedup();
    v.shrink_to_fit();
    Ok(v)
}

/// Streaming iterator over the live `Add` entries of a CDX prefix (tombstones applied), in CDX
/// order. Lending: each entry borrows the iterator's line buffer (`Entry::into_owned` keeps it).
pub struct Entries {
    lines: LineReader,
    tombstones: Vec<(u32, u64)>,
}

impl Entries {
    /// Next live `Add` entry and its CDX offset, or `None` at the end of the prefix.
    ///
    /// # Errors
    ///
    /// `Io`, `CdxTruncated` (the file shrank), `CdxCorrupt` (a line of the fsynced prefix is
    /// invalid).
    pub fn next_entry(&mut self) -> Result<Option<(u64, Entry<'_>)>, Error> {
        let Entries { lines, tombstones } = self;
        loop {
            let Some(at) = lines.advance()? else {
                return Ok(None);
            };
            let keep = {
                let e = lines.parse_current(at)?;
                e.kind == Kind::Add && tombstones.binary_search(&(e.segment, e.offset)).is_err()
            };
            if keep {
                // The kept line is parsed twice (about 200 ns) to keep a lending API without
                // `unsafe`.
                let e = lines.parse_current(at)?;
                return Ok(Some((at, e)));
            }
        }
    }

    /// Number of distinct tombstoned locators applied (16 bytes of RAM each).
    #[must_use]
    pub fn tombstones(&self) -> usize {
        self.tombstones.len()
    }
}

/// One line of a CDX range, as read. Lending: `entry` borrows the iterator's buffer.
#[derive(Debug)]
pub struct CdxLine<'a> {
    /// Offset of the line.
    pub at: u64,
    /// Offset just after the line (its `\n`): the resume point after applying it.
    pub next: u64,
    /// The parsed entry (`Kind::Add` or `Kind::Tombstone`).
    pub entry: Entry<'a>,
}

/// Streaming iterator over every line (Add and Tombstone, no filtering) of a fsynced CDX range,
/// in order.
pub struct CdxLines {
    lines: LineReader,
}

impl CdxLines {
    /// Next line, or `None` exactly at `end`.
    ///
    /// # Errors
    ///
    /// `Io`, `CdxTruncated` (the file shrank, or `end` is inside a line), `CdxCorrupt`.
    pub fn next_line(&mut self) -> Result<Option<CdxLine<'_>>, Error> {
        let Some(at) = self.lines.advance()? else {
            return Ok(None);
        };
        let next = self.lines.next_offset();
        let entry = self.lines.parse_current(at)?;
        Ok(Some(CdxLine { at, next, entry }))
    }

    /// Offset of the next unread line (= `from` before the first call, = `end` at the end).
    #[must_use]
    pub fn position(&self) -> u64 {
        self.lines.next_offset()
    }
}

impl Snapshot {
    /// Every line of this generation's CDX in `[from, end)`. `from = None` starts right after
    /// the header. `from` must be a line start (or `end`), and `end` must be fsynced,
    /// line-aligned data (`Writer::cdx_len()` or an index checkpoint taken from it). One
    /// sequential pass. RAM: 64 KiB plus one line.
    ///
    /// # Errors
    ///
    /// `CdxMissing`, `CdxRead`, `CdxGeneration`, `CdxRange` (`from` below the header,
    /// `from > end`, or `from` not at a line start), `CdxTruncated` (`end` beyond the file), `Io`.
    pub fn cdx_lines(&self, from: Option<u64>, end: u64) -> Result<CdxLines, Error> {
        let lines = LineReader::open(&self.cdx_path(), self.generation(), from, end)?;
        Ok(CdxLines { lines })
    }

    /// Live entries of this generation's CDX prefix `[header, end)`: every `Add` line whose
    /// `(segment, offset)` is not named by a `T` line of the prefix, in CDX order (= locator
    /// order). The warcinfo entry is included. `end` must be a line boundary of fsynced CDX data:
    /// pass `Writer::cdx_len()` (or use `Collection::entries`); lines appended later are not
    /// seen, so the result is a consistent prefix even while the writer appends. Two sequential
    /// passes over the prefix; RAM: 16 bytes per distinct tombstone, 64 KiB and one line.
    ///
    /// # Errors
    ///
    /// `CdxMissing`, `CdxRead`, `CdxGeneration`, `CdxRange` (`end` below the header), `CdxTruncated`
    /// (`end` beyond the file or inside a line), `CdxCorrupt`, `Io`, `Internal`.
    pub fn entries(&self, end: u64) -> Result<Entries, Error> {
        let path = self.cdx_path();
        let generation = self.generation();
        let mut p1 = LineReader::open(&path, generation, None, end)?;
        let tombstones = collect_tombstones(&mut p1)?;
        drop(p1);
        let lines = LineReader::open(&path, generation, None, end)?;
        Ok(Entries { lines, tombstones })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::Collection;
    use crate::prepare::Preparer;
    use crate::testutil::{
        TestDir, cdx_lines, commit_batch, creators, drain, live, open_ok, read_bytes, record,
        with_timeout,
    };
    use masstuffy_cdx::{DEFAULT_MAX_LINE, encode_entry};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct Scenario {
        _d: TestDir,
        dir: PathBuf,
        c: Collection,
        wi: Entry<'static>,
        /// Entries of records 1..=12 (index 0 is record 1).
        es: Vec<Entry<'static>>,
        inputs: HashMap<String, Vec<u8>>,
        cdx_before_t9: u64,
    }

    fn batch(
        c: &mut Collection,
        p: &mut Preparer,
        inputs: &mut HashMap<String, Vec<u8>>,
        ids: std::ops::RangeInclusive<usize>,
    ) -> Vec<Entry<'static>> {
        let recs: Vec<_> = ids.map(|i| record(i, 40 + i * 7)).collect();
        for r in &recs {
            inputs.insert(
                r.view().record_id().unwrap().to_owned(),
                r.as_bytes().to_vec(),
            );
        }
        commit_batch(c, p, recs)
    }

    fn scenario(create: crate::testutil::Create) -> Scenario {
        let d = TestDir::new();
        let dir = d.sub();
        let (mut c, wi) = create(&dir);
        let mut p = c.preparer().unwrap();
        let mut inputs = HashMap::new();
        let mut es = batch(&mut c, &mut p, &mut inputs, 1..=3);
        es.extend(batch(&mut c, &mut p, &mut inputs, 4..=7));
        es.extend(batch(&mut c, &mut p, &mut inputs, 8..=10));
        c.tombstone(&[es[1].clone(), es[4].clone()]).unwrap();
        es.extend(batch(&mut c, &mut p, &mut inputs, 11..=12));
        let cdx_before_t9 = c.writer().cdx_len();
        c.tombstone(&[es[8].clone(), es[1].clone()]).unwrap();
        Scenario {
            _d: d,
            dir,
            c,
            wi,
            es,
            inputs,
            cdx_before_t9,
        }
    }

    fn pick(s: &Scenario, nums: &[usize]) -> Vec<Entry<'static>> {
        let mut v = vec![s.wi.clone()];
        v.extend(nums.iter().map(|n| s.es[n - 1].clone()));
        v
    }

    #[test]
    fn entries_interleaved_tombstones() {
        for (_, create) in creators() {
            let s = scenario(create);
            let want = pick(&s, &[1, 3, 4, 6, 7, 8, 10, 11, 12]);
            let lines = cdx_lines(&s.dir, 1);
            let mut it = s.c.entries().unwrap();
            let mut got = Vec::new();
            while let Some((at, e)) = it.next_entry().unwrap() {
                got.push((at, e.into_owned()));
            }
            assert_eq!(it.tombstones(), 3);
            let entries: Vec<_> = got.iter().map(|g| g.1.clone()).collect();
            assert_eq!(entries, want);
            for (at, e) in &got {
                let line = lines.iter().find(|l| l.0 == *at).unwrap();
                assert_eq!(&line.2, e);
            }
            for e in entries.iter().skip(1) {
                assert_eq!(read_bytes(s.c.snapshot(), e), s.inputs[&*e.record_id]);
            }
            assert_eq!(live(&s.c), want);
        }
    }

    #[test]
    fn entries_prefix_bound() {
        for (_, create) in creators() {
            let mut s = scenario(create);
            let snap = Arc::clone(s.c.snapshot());
            // Before the second tombstone call: record 9 and the second `T` for 2 are not seen.
            let got = drain(&snap, s.cdx_before_t9);
            assert!(got.contains(&s.es[8]));
            assert!(!got.contains(&s.es[1]));
            // Lines committed after a captured length are not returned.
            let captured = s.c.writer().cdx_len();
            let mut p = s.c.preparer().unwrap();
            commit_batch(&mut s.c, &mut p, vec![record(13, 10)]);
            let got = drain(&snap, captured);
            assert!(
                got.iter()
                    .all(|e| *e.record_id != crate::testutil::id_str(13))
            );
            let got = drain(&snap, s.c.writer().cdx_len());
            assert!(
                got.iter()
                    .any(|e| *e.record_id == crate::testutil::id_str(13))
            );
        }
    }

    #[test]
    fn entries_after_reopen() {
        for (_, create) in creators() {
            let s = scenario(create);
            let want = live(&s.c);
            let Scenario { _d, dir, c, es, .. } = s;
            let e1 = es[0].clone();
            drop(c);
            let (c, rep) = open_ok(&dir);
            assert!(rep.is_clean());
            assert_eq!(live(&c), want);
            drop(c);
            // A torn `T` line (half of it) is truncated by `open`; the record stays live.
            let mut line = Vec::new();
            let t = Entry {
                kind: Kind::Tombstone,
                ..e1.clone()
            };
            encode_entry(&t, DEFAULT_MAX_LINE, &mut line).unwrap();
            let f = std::fs::OpenOptions::new()
                .append(true)
                .open(dir.join("index.1.cdx"))
                .unwrap();
            std::io::Write::write_all(&mut &f, &line[..line.len() / 2]).unwrap();
            drop(f);
            let (c, rep) = open_ok(&dir);
            assert!(rep.cdx_torn.is_some());
            assert_eq!(live(&c), want);
            assert!(live(&c).contains(&e1));
        }
    }

    #[test]
    fn entries_concurrent_with_commits() {
        with_timeout(60, || {
            let d = TestDir::new();
            let (c, _) = crate::testutil::create_zstd(&d.sub());
            let mut p = c.preparer().unwrap();
            let (mut writer, snap) = c.into_parts();
            let published = AtomicU64::new(writer.cdx_len());
            let done = AtomicBool::new(false);
            let all: Mutex<Vec<Entry<'static>>> = Mutex::new(Vec::new());
            std::thread::scope(|s| {
                s.spawn(|| {
                    for i in 1..=50 {
                        let prepared = vec![p.prepare(record(i, 30 + i)).unwrap()];
                        let es = writer.commit(&prepared).unwrap();
                        all.lock().unwrap().extend(es);
                        published.store(writer.cdx_len(), Ordering::Release);
                    }
                    done.store(true, Ordering::Release);
                });
                for _ in 0..3 {
                    s.spawn(|| {
                        let mut last = 0usize;
                        loop {
                            let finished = done.load(Ordering::Acquire);
                            let v = published.load(Ordering::Acquire);
                            let got = drain(&snap, v);
                            assert!(got.len() >= last);
                            last = got.len();
                            for e in &got {
                                read_bytes(&snap, e);
                            }
                            if finished {
                                break;
                            }
                        }
                        assert_eq!(last, 51);
                    });
                }
            });
            assert_eq!(all.lock().unwrap().len(), 50);
        });
    }

    fn all_lines(
        snap: &Snapshot,
        from: Option<u64>,
        end: u64,
    ) -> (Vec<(u64, u64, Entry<'static>)>, u64) {
        let mut it = snap.cdx_lines(from, end).unwrap();
        let mut v = Vec::new();
        while let Some(l) = it.next_line().unwrap() {
            v.push((l.at, l.next, l.entry.into_owned()));
        }
        (v, it.position())
    }

    #[test]
    fn cdx_lines_all_kinds_in_order() {
        for (_, create) in creators() {
            let mut s = scenario(create);
            let want = cdx_lines(&s.dir, 1);
            let len = s.c.writer().cdx_len();
            let (got, pos) = all_lines(s.c.snapshot(), None, len);
            assert_eq!(got, want);
            assert_eq!(pos, len);
            assert!(got.iter().any(|l| l.2.kind == Kind::Tombstone));
            assert!(got.iter().any(|l| l.2.kind == Kind::Add));
        }
    }

    #[test]
    fn cdx_lines_from_middle() {
        for (_, create) in creators() {
            let mut s = scenario(create);
            let want = cdx_lines(&s.dir, 1);
            let len = s.c.writer().cdx_len();
            let (got, _) = all_lines(s.c.snapshot(), Some(want[3].0), len);
            assert_eq!(got, want[3..]);
            let (got, pos) = all_lines(s.c.snapshot(), Some(len), len);
            assert!(got.is_empty());
            assert_eq!(pos, len);
        }
    }

    #[test]
    fn cdx_lines_bad_bounds() {
        let mut s = scenario(crate::testutil::create_zstd);
        let want = cdx_lines(&s.dir, 1);
        let len = s.c.writer().cdx_len();
        let snap = s.c.snapshot();
        let r = |from, end| snap.cdx_lines(from, end).map(|_| ());
        assert!(matches!(
            r(Some(want[3].0 + 2), len),
            Err(Error::CdxRange { .. })
        ));
        assert!(matches!(
            r(Some(want[0].0 - 1), len),
            Err(Error::CdxRange { .. })
        ));
        assert!(matches!(r(Some(len), len - 1), Err(Error::CdxRange { .. })));
        assert!(matches!(r(None, len + 10), Err(Error::CdxTruncated { .. })));
        let mut it = snap.cdx_lines(None, want[2].0 + 3).unwrap();
        it.next_line().unwrap().unwrap();
        it.next_line().unwrap().unwrap();
        assert!(matches!(it.next_line(), Err(Error::CdxTruncated { .. })));
        // Wrong generation: a snapshot of generation 2 pointing at the same file name is not
        // available here, so check through the header-generation error path of the reader.
        let path = snap.cdx_path();
        assert!(matches!(
            LineReader::open(&path, 7, None, len).map(|_| ()),
            Err(Error::CdxGeneration { .. })
        ));
    }
}
