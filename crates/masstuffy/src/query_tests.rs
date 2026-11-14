//! M19 tests: envelope import, scan, search, lookups.

use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use masstuffy_cdx::{Entry, EnvelopeError};
use masstuffy_collection::{Codec, CollectionId, PrepareError, RawCheck, RawEncoding};
use masstuffy_warc::{Record, RecordBuilder, WarcDate};

use crate::collection::CollectionHandle;
use crate::config::Config;
use crate::error::{Error, ErrorKind};
use crate::import::{ImportOptions, ImportReport, MAX_IMPORT_BATCH_RECORDS, OnInvalid};
use crate::query::{SearchCursor, SearchQuery};
use crate::read::{Fetch, Fetched};
use crate::repository::Repository;
use crate::scan::{MAX_SCAN_PAGE, ScanCursor, ScanOptions};
use crate::testutil::*;
use crate::types::{Located, RawItem};
use masstuffy_urlkey::{HostMatch, PathFilter, PortFilter, UrlPattern};

macro_rules! rt_test {
    ($(#[$m:meta])* async fn $name:ident() $body:block) => {
        $(#[$m])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            bounded(async $body).await
        }
    };
}

async fn open_with(dir: &Path, check: RawCheck) -> Repository {
    Repository::open_inner(dir.to_path_buf(), check)
        .await
        .unwrap()
        .0
}

async fn fresh(
    check: RawCheck,
    codec: Codec,
    config: &Config,
) -> (TestDir, Repository, CollectionHandle) {
    let td = TestDir::new();
    prepare_repo(&td.repo(), config).await;
    let repo = open_with(&td.repo(), check).await;
    let h = repo.create_collection("c", codec).await.unwrap();
    (td, repo, h)
}

fn loc(h: &CollectionHandle, e: &Entry<'static>) -> Located {
    Located {
        collection: h.id(),
        generation: h.info().generation,
        entry: e.clone(),
    }
}

fn import_failure(r: Result<ImportReport, Error>) -> (Box<ImportReport>, Box<Error>) {
    match r {
        Err(Error::Import { report, source }) => (report, source),
        other => panic!("expected Error::Import, got {other:?}"),
    }
}

fn ids(entries: &[Entry<'static>]) -> Vec<String> {
    entries.iter().map(|e| e.record_id.to_string()).collect()
}

fn id_strs(range: impl IntoIterator<Item = usize>) -> Vec<String> {
    range.into_iter().map(id_str).collect()
}

fn cursor_of(bytes: &[u8]) -> Cursor<Vec<u8>> {
    Cursor::new(bytes.to_vec())
}

/// A zstd item (frame of the record) for a Zstd collection.
fn zstd_item(i: usize, body_len: usize) -> crate::testutil::Item {
    let rec = record(i, body_len);
    let payload = frame(base_dict(), rec.as_bytes());
    (meta_for(&rec, &payload, true), payload)
}

fn record_at(i: usize, warc_type: &str, url: &str, date: &str) -> Record {
    let mut b = RecordBuilder::new(warc_type).unwrap();
    b.record_id(&id_str(i)).unwrap();
    b.date(WarcDate::parse(date).unwrap());
    b.target_uri(url).unwrap();
    b.build(&letters(i, 40)).unwrap()
}

fn pattern(host: &str, hm: HostMatch, port: PortFilter, path: PathFilter<'_>) -> UrlPattern {
    UrlPattern::new(host, hm, port, path).unwrap()
}

async fn search_ids(h: &CollectionHandle, p: &UrlPattern) -> BTreeSet<String> {
    let page = h
        .search(
            p,
            &SearchQuery {
                limit: 1000,
                after: None,
            },
        )
        .await
        .unwrap();
    assert!(page.next.is_none());
    page.hits
        .iter()
        .map(|l| l.entry.record_id.to_string())
        .collect()
}

// ---- Envelope import.

rt_test! {
async fn envelope_v1_import_none_full() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let items: Vec<_> = (1..=10).map(|i| plain_item(i, 50 + i)).collect();
    let (bytes, _) = envelope_v1(&items);
    let rep = h
        .put_raw_envelope(cursor_of(&bytes), ImportOptions::default())
        .await
        .unwrap();
    assert_eq!(rep.format, Some(crate::EnvelopeFormat::V1));
    assert_eq!((rep.records, rep.rejected), (10, 0));
    assert_eq!(rep.committed_offset, bytes.len() as u64);
    let live = live_entries(&h).await;
    assert_eq!(live.len(), 11);
    assert_eq!(ids(&live[1..]), id_strs(1..=10));
    for (e, (_, payload)) in live[1..].iter().zip(&items) {
        assert_eq!(&repo.get(&loc(&h, e)).await.unwrap(), payload);
    }
    repo.sync_index().await.unwrap();
    for i in 1..=10 {
        let f = repo.find_by_id(&id_str(i), Some(h.id())).await.unwrap();
        assert!(f.is_some());
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn envelope_legacy_import_zstd_full() {
    let (_td, repo, src) = fresh(RawCheck::Full, zstd_codec(), &Config::default()).await;
    let dst = repo.create_collection("dst", zstd_codec()).await.unwrap();
    let recs: Vec<Record> = (1..=6).map(|i| record(i, 100 + i)).collect();
    let located = src.put(recs.clone()).await.unwrap();
    let mut items = Vec::new();
    for (rec, at) in recs.iter().zip(&located) {
        let raw = repo.get_raw(at).await.unwrap();
        items.push((meta_for(rec, &raw.bytes, false), raw.bytes));
    }
    let (bytes, _) = envelope_legacy(&items);
    let rep = dst
        .put_raw_envelope(cursor_of(&bytes), ImportOptions::default())
        .await
        .unwrap();
    assert_eq!(rep.format, Some(crate::EnvelopeFormat::Legacy));
    assert_eq!(rep.records, 6);
    let live = live_entries(&dst).await;
    for (e, rec) in live[1..].iter().zip(&recs) {
        assert_eq!(repo.get(&loc(&dst, e)).await.unwrap(), rec.as_bytes());
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn envelope_wrong_dict_id_rejected() {
    for check in [RawCheck::Full, RawCheck::HeaderOnly] {
        let (_td, repo, h) = fresh(check, zstd_codec(), &Config::default()).await;
        let other = repo.create_collection("other", zstd_codec()).await.unwrap();
        let rec2 = record(2, 80);
        let bad_payload = frame(&other_dict(), rec2.as_bytes());
        let bad = (meta_for(&rec2, &bad_payload, true), bad_payload);
        let items = vec![zstd_item(1, 80), bad, zstd_item(3, 80)];
        let (bytes, offsets) = envelope_v1(&items);

        let r = h.put_raw_envelope(cursor_of(&bytes), ImportOptions::default()).await;
        let (report, source) = import_failure(r);
        match &*source {
            Error::Prepare {
                index: 1,
                source: PrepareError::UnexpectedDictId { frame, expected },
            } => assert_eq!((*frame, *expected), (other_dict_id(), dict_id())),
            e => panic!("{e:?}"),
        }
        assert_eq!(report.records, 1);
        assert_eq!(report.committed_offset, offsets[1]);
        assert_eq!(ids(&live_entries(&h).await[1..]), id_strs([1]));

        let opts = ImportOptions { on_invalid: OnInvalid::Skip, ..ImportOptions::default() };
        let rep = other.put_raw_envelope(cursor_of(&bytes), opts).await.unwrap();
        assert_eq!((rep.records, rep.rejected), (2, 1));
        assert_eq!(rep.committed_offset, bytes.len() as u64);
        let rj = &rep.rejections[0];
        assert_eq!((rj.offset, rj.ordinal), (offsets[1], 1));
        assert_eq!(rj.record_id, id_str(2));
        assert!(matches!(rj.error, PrepareError::UnexpectedDictId { .. }), "{:?}", rj.error);
        assert_eq!(ids(&live_entries(&other).await[1..]), id_strs([1, 3]));
        repo.shutdown().await.unwrap();
    }
}
}

rt_test! {
async fn envelope_small_plain_frame_rejected() {
    for check in [RawCheck::Full, RawCheck::HeaderOnly] {
        let (_td, repo, h) = fresh(check, zstd_codec(), &Config::default()).await;
        let rec = record(1, 80);
        let payload = plain_frame(rec.as_bytes());
        let items = vec![(meta_for(&rec, &payload, true), payload)];
        let (bytes, _) = envelope_v1(&items);
        let r = h.put_raw_envelope(cursor_of(&bytes), ImportOptions::default()).await;
        let (report, source) = import_failure(r);
        assert!(
            matches!(&*source, Error::Prepare { index: 0, source: PrepareError::PlainFrameNotAllowed { .. } }),
            "{source:?}"
        );
        assert_eq!(report.records, 0);
        assert_eq!(live_entries(&h).await.len(), 1);
        repo.shutdown().await.unwrap();
    }
}
}

rt_test! {
async fn envelope_header_only_mismatch_import() {
    let (_td, repo, h) = fresh(RawCheck::HeaderOnly, Codec::None, &Config::default()).await;
    let payload = record(1, 100).as_bytes().to_vec();
    let mut meta = meta_for(&record(1, 100), &payload, true);
    meta.record_id = id_str(2).into();
    let items = vec![(meta, payload.clone())];
    let (bytes, _) = envelope_v1(&items);
    h.put_raw_envelope(cursor_of(&bytes), ImportOptions::default()).await.unwrap();
    repo.sync_index().await.unwrap();
    let e = repo.get_by_id(&id_str(2), Some(h.id())).await.err().unwrap();
    assert!(matches!(e, Error::NotDecodable { .. }), "{e:?}");
    let at = repo.find_by_id(&id_str(2), Some(h.id())).await.unwrap().unwrap();
    let raw = repo.get_raw(&at).await.unwrap();
    assert_eq!(raw.bytes, payload);
    assert_eq!(raw.encoding, RawEncoding::Warc);
    repo.shutdown().await.unwrap();

    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let r = h.put_raw_envelope(cursor_of(&bytes), ImportOptions::default()).await;
    let (_, source) = import_failure(r);
    assert!(
        matches!(&*source, Error::Prepare { source: PrepareError::MetaMismatch(_), .. }),
        "{source:?}"
    );
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn envelope_size_cap() {
    let config = Config { max_record_bytes: 4096, ..Config::default() };
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &config).await;
    let items = vec![plain_item(1, 50)];
    let (mut bytes, offsets) = envelope_legacy(&items);
    let second = bytes.len() as u64;
    bytes.extend_from_slice(
        format!("- response {} 20261113100000 f 0 1048576\n", id_str(2)).as_bytes(),
    );
    bytes.extend_from_slice(&vec![b'x'; 1 << 20]);
    assert_eq!(offsets, [0]);
    let r = h.put_raw_envelope(cursor_of(&bytes), ImportOptions::default()).await;
    let (report, source) = import_failure(r);
    assert!(
        matches!(&*source, Error::Envelope(EnvelopeError::RecordTooLarge { .. })),
        "{source:?}"
    );
    assert_eq!(report.records, 1);
    assert_eq!(report.committed_offset, second);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn envelope_batching() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let items: Vec<_> = (1..=10).map(|i| plain_item(i, 50)).collect();
    let (bytes, _) = envelope_v1(&items);
    let commits = h.stats().commits;
    let opts = ImportOptions { batch_records: 3, ..ImportOptions::default() };
    let rep = h.put_raw_envelope(cursor_of(&bytes), opts).await.unwrap();
    assert_eq!((rep.records, rep.batches), (10, 4));
    assert!(h.stats().commits >= commits + 4);

    let items: Vec<_> = (11..=15).map(|i| plain_item(i, 50)).collect();
    let len = u64::try_from(items[0].1.len()).unwrap();
    let (bytes, _) = envelope_v1(&items);
    let opts = ImportOptions { batch_bytes: Some(2 * len), ..ImportOptions::default() };
    let rep = h.put_raw_envelope(cursor_of(&bytes), opts).await.unwrap();
    assert_eq!((rep.records, rep.batches), (5, 3));

    let before = live_entries(&h).await.len();
    for opts in [
        ImportOptions { batch_records: 0, ..ImportOptions::default() },
        ImportOptions { batch_records: MAX_IMPORT_BATCH_RECORDS + 1, ..ImportOptions::default() },
        ImportOptions { batch_bytes: Some(0), ..ImportOptions::default() },
    ] {
        let e = h.put_raw_envelope(cursor_of(&bytes), opts).await.unwrap_err();
        assert!(matches!(e, Error::InvalidQuery(_)), "{e:?}");
    }
    assert_eq!(live_entries(&h).await.len(), before);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn envelope_truncated_commits_prefix_and_resumes() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let items: Vec<_> = (1..=5).map(|i| plain_item(i, 60)).collect();
    let (full, offsets) = envelope_v1(&items);
    let cut = &full[..full.len() - 3];
    let r = h.put_raw_envelope(cursor_of(cut), ImportOptions::default()).await;
    let (report, source) = import_failure(r);
    assert!(matches!(&*source, Error::Envelope(EnvelopeError::Truncated { .. })), "{source:?}");
    assert_eq!(report.records, 4);
    assert_eq!(report.committed_offset, offsets[4]);

    let header_end = full.iter().position(|b| *b == b'\n').unwrap() + 1;
    let mut resume = full[..header_end].to_vec();
    resume.extend_from_slice(&full[usize::try_from(report.committed_offset).unwrap()..]);
    let rep = h
        .put_raw_envelope(Cursor::new(resume), ImportOptions::default())
        .await
        .unwrap();
    assert_eq!(rep.records, 1);
    let live = live_entries(&h).await;
    assert_eq!(ids(&live[1..]), id_strs(1..=5));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn envelope_bad_line_mid_stream() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let items: Vec<_> = (1..=5).map(|i| plain_item(i, 60)).collect();
    let (mut bytes, offsets) = envelope_v1(&items);
    let at = usize::try_from(offsets[2]).unwrap() + 3;
    bytes[at] = if bytes[at] == b'z' { b'y' } else { b'z' };
    let r = h.put_raw_envelope(cursor_of(&bytes), ImportOptions::default()).await;
    let (report, source) = import_failure(r);
    assert!(matches!(&*source, Error::Envelope(EnvelopeError::Line { .. })), "{source:?}");
    assert_eq!(report.records, 2);
    assert_eq!(report.committed_offset, offsets[2]);
    assert_eq!(ids(&live_entries(&h).await[1..]), id_strs(1..=2));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn envelope_empty_and_bad_header() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let rep = h.put_raw_envelope(cursor_of(b""), ImportOptions::default()).await.unwrap();
    assert_eq!((rep.records, rep.format, rep.committed_offset), (0, None, 0));
    let r = h
        .put_raw_envelope(cursor_of(b"#masstuffy-cdx/9 gen=0\n"), ImportOptions::default())
        .await;
    let (report, source) = import_failure(r);
    assert!(matches!(&*source, Error::Envelope(EnvelopeError::Header(_))), "{source:?}");
    assert_eq!(report.records, 0);
    repo.shutdown().await.unwrap();
}
}

type Chan = (
    std::sync::mpsc::Sender<Vec<u8>>,
    std::sync::mpsc::Receiver<u64>,
    ChanReader,
);

fn chan_reader() -> Chan {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (st_tx, st_rx) = std::sync::mpsc::channel::<u64>();
    (
        tx,
        st_rx,
        ChanReader {
            rx,
            cur: Vec::new(),
            pos: 0,
            consumed: 0,
            starved: Some(st_tx),
        },
    )
}

async fn wait_records(h: &CollectionHandle, n: u64) {
    while h.stats().records < n {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

async fn wait_starved(st: &std::sync::mpsc::Receiver<u64>, consumed: u64) {
    loop {
        while let Ok(v) = st.try_recv() {
            if v == consumed {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

rt_test! {
async fn envelope_cancel_mid_import() {
    let (td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let items: Vec<_> = (1..=4).map(|i| plain_item(i, 60)).collect();
    let (bytes, offsets) = envelope_v1(&items);
    let (tx, starved, reader) = chan_reader();
    let base = h.stats().records;
    let h2 = h.clone();
    let opts = ImportOptions { batch_records: 2, ..ImportOptions::default() };
    let task = tokio::spawn(async move { h2.put_raw_envelope(reader, opts).await });
    let first = usize::try_from(offsets[2]).unwrap();
    tx.send(bytes[..first].to_vec()).unwrap();
    wait_records(&h, base + 2).await;
    let mid = first + (usize::try_from(offsets[3]).unwrap() - first) / 2;
    tx.send(bytes[first..mid].to_vec()).unwrap();
    wait_starved(&starved, u64::try_from(mid).unwrap()).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(tx);
    repo.shutdown().await.unwrap();
    drop((repo, h));
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection("c").unwrap();
    assert_eq!(ids(&live_entries(&h).await[1..]), id_strs(1..=2));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn envelope_shutdown_mid_import() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let items: Vec<_> = (1..=4).map(|i| plain_item(i, 60)).collect();
    let (bytes, offsets) = envelope_v1(&items);
    let (tx, _starved, reader) = chan_reader();
    let base = h.stats().records;
    let h2 = h.clone();
    let opts = ImportOptions { batch_records: 2, ..ImportOptions::default() };
    let task = tokio::spawn(async move { h2.put_raw_envelope(reader, opts).await });
    let first = usize::try_from(offsets[2]).unwrap();
    tx.send(bytes[..first].to_vec()).unwrap();
    wait_records(&h, base + 2).await;
    let r2 = repo.clone();
    let sd = tokio::spawn(async move { r2.shutdown().await });
    while !*repo.core().gate.read().await {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    tx.send(bytes[first..].to_vec()).unwrap();
    drop(tx);
    let r = task.await.unwrap();
    let (report, source) = import_failure(r);
    assert_eq!(source.kind(), ErrorKind::Unavailable, "{source:?}");
    assert_eq!(report.records, 2);
    sd.await.unwrap().unwrap();
}
}

rt_test! {
async fn envelope_concurrent_with_puts() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &small_config()).await;
    let mut joins = Vec::new();
    for imp in 0..2usize {
        let h = h.clone();
        joins.push(tokio::spawn(async move {
            let items: Vec<_> = (0..50).map(|j| plain_item(1000 * (imp + 1) + j, 40)).collect();
            let (bytes, _) = envelope_v1(&items);
            let opts = ImportOptions { batch_records: 7, ..ImportOptions::default() };
            h.put_raw_envelope(Cursor::new(bytes), opts).await.unwrap().records
        }));
    }
    let mut putters = Vec::new();
    for t in 0..4usize {
        let h = h.clone();
        putters.push(tokio::spawn(async move {
            for j in 0..25 {
                h.put(vec![record(3000 + t * 100 + j, 40)]).await.unwrap();
            }
        }));
    }
    for j in joins {
        assert_eq!(j.await.unwrap(), 50);
    }
    for p in putters {
        p.await.unwrap();
    }
    let live = live_entries(&h).await;
    assert_eq!(live.len(), 201);
    let all = ids(&live);
    assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), 201);
    for imp in 0..2usize {
        for batch in 0..8usize {
            let lo = batch * 7;
            let hi = (lo + 7).min(50);
            let pos: Vec<usize> = (lo..hi)
                .map(|j| all.iter().position(|id| *id == id_str(1000 * (imp + 1) + j)).unwrap())
                .collect();
            assert!(
                pos.windows(2).all(|w| w[1] == w[0] + 1),
                "import {imp} batch {batch} not contiguous: {pos:?}"
            );
        }
    }
    repo.shutdown().await.unwrap();
}
}

// ---- Scan.

rt_test! {
async fn scan_returns_live_entries_in_cdx_order() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    for r in [1..=5, 6..=9, 10..=12] {
        h.put(r.map(|i| record(i, 40)).collect()).await.unwrap();
    }
    let opts = ScanOptions { page_records: 4, ..ScanOptions::default() };
    let mut scan = h.scan(opts).unwrap();
    let mut sizes = Vec::new();
    let mut all = Vec::new();
    let mut cursors = Vec::new();
    while let Some(p) = scan.next_page().await.unwrap() {
        sizes.push(p.items.len());
        cursors.push(p.next);
        for it in p.items {
            assert!(it.record.is_none());
            assert_eq!(it.located.generation, h.info().generation);
            all.push(it.located.entry);
        }
    }
    assert_eq!(sizes, [4, 4, 4, 1]);
    assert!(scan.next_page().await.unwrap().is_none());
    assert_eq!(all, live_entries(&h).await);
    let c = ScanCursor::from_token(&cursors[1].to_token()).unwrap();
    assert_eq!(c, cursors[1]);
    let mut scan = h.scan(ScanOptions { after: Some(c), ..opts }).unwrap();
    let rest: Vec<_> = drain_scan(&mut scan).await.into_iter().map(|i| i.located.entry).collect();
    assert_eq!(rest, all[8..]);
    assert_eq!(scan.cursor().map(|c| c.to_token()), Some(cursors[3].to_token()));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn scan_fetch_decoded_and_raw() {
    let (_td, repo, h) = fresh(RawCheck::Full, zstd_codec(), &Config::default()).await;
    let recs: Vec<Record> = (1..=5).map(|i| record(i, 100)).collect();
    h.put(recs.clone()).await.unwrap();
    let mut scan = h.scan(ScanOptions { fetch: Fetch::Decoded, ..ScanOptions::default() }).unwrap();
    let items = drain_scan(&mut scan).await;
    assert_eq!(items.len(), 6);
    assert!(matches!(items[0].record, Some(Ok(Fetched::Decoded(_)))));
    for (it, rec) in items[1..].iter().zip(&recs) {
        match &it.record {
            Some(Ok(Fetched::Decoded(v))) => assert_eq!(v, rec.as_bytes()),
            o => panic!("{o:?}"),
        }
    }
    let mut scan = h.scan(ScanOptions { fetch: Fetch::Raw, ..ScanOptions::default() }).unwrap();
    for it in drain_scan(&mut scan).await {
        let want = repo.get_raw(&it.located).await.unwrap();
        match it.record {
            Some(Ok(Fetched::Raw(r))) => assert_eq!((r.bytes, r.encoding), (want.bytes, want.encoding)),
            o => panic!("{o:?}"),
        }
    }
    let opts = ScanOptions { fetch: Fetch::Decoded, page_bytes: 1, ..ScanOptions::default() };
    let mut scan = h.scan(opts).unwrap();
    let mut pages = 0;
    while let Some(p) = scan.next_page().await.unwrap() {
        assert_eq!(p.items.len(), 1);
        pages += 1;
    }
    assert_eq!(pages, 6);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn scan_skips_orphan_bytes() {
    let (td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    h.put((1..=5).map(|i| record(i, 100)).collect()).await.unwrap();
    let dir = td.repo().join("data/repository").join(h.id().to_string());
    repo.shutdown().await.unwrap();
    drop((repo, h));
    let seg = last_segment(&dir);
    let before = std::fs::metadata(&seg).unwrap().len();
    let mut orphan = record(99, 2000).as_bytes().to_vec();
    assert!(orphan.len() < 3000);
    orphan.resize(3000, 0xFF);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&seg).unwrap();
        f.write_all(&orphan).unwrap();
        f.sync_all().unwrap();
    }
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection("c").unwrap();
    h.put((6..=8).map(|i| record(i, 100)).collect()).await.unwrap();
    let mut scan = h.scan(ScanOptions { fetch: Fetch::Decoded, ..ScanOptions::default() }).unwrap();
    let items = drain_scan(&mut scan).await;
    assert_eq!(items.len(), 9);
    assert!(items.iter().all(|i| matches!(i.record, Some(Ok(_)))));
    let got: Vec<_> = items.iter().map(|i| i.located.entry.record_id.to_string()).collect();
    assert_eq!(got[1..], id_strs(1..=8));
    assert!(!got.contains(&id_str(99)));
    let first_new = &items[6].located.entry;
    let seg_no: u32 = seg
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split('.').nth(2))
        .and_then(|n| n.parse().ok())
        .unwrap();
    assert_eq!(first_new.segment, seg_no);
    assert!(first_new.offset >= before + 3000, "{} < {}", first_new.offset, before + 3000);
    repo.sync_index().await.unwrap();
    assert!(repo.find_by_id(&id_str(99), None).await.unwrap().is_none());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn scan_skips_tombstoned() {
    let (td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    h.put((1..=6).map(|i| record(i, 50)).collect()).await.unwrap();
    let live = live_entries(&h).await;
    let off = offline_ctx(&repo, &h);
    repo.shutdown().await.unwrap();
    drop((repo, h));
    offline_tombstone(&off, &[live[2].clone(), live[5].clone()]);
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection("c").unwrap();
    let mut scan = h.scan(ScanOptions::default()).unwrap();
    let got: Vec<_> = drain_scan(&mut scan).await.into_iter().map(|i| i.located.entry).collect();
    assert_eq!(ids(&got[1..]), id_strs([1, 3, 4, 6]));
    repo.sync_index().await.unwrap();
    assert!(repo.find_by_id(&id_str(2), None).await.unwrap().is_none());
    let p = pattern("example.org", HostMatch::Exact, PortFilter::Any, PathFilter::Any);
    let hits = search_ids(&h, &p).await;
    assert_eq!(hits, id_strs([1, 3, 4, 6]).into_iter().collect());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn scan_resume_cursor_tombstone_line_before_from() {
    let (td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    h.put((1..=6).map(|i| record(i, 50)).collect()).await.unwrap();
    let live = live_entries(&h).await;
    let mut scan = h.scan(ScanOptions { page_records: 3, ..ScanOptions::default() }).unwrap();
    let page = scan.next_page().await.unwrap().unwrap();
    assert_eq!(page.items.len(), 3);
    let c = page.next;
    drop(scan);
    let off = offline_ctx(&repo, &h);
    repo.shutdown().await.unwrap();
    drop((repo, h));
    offline_tombstone(&off, &[live[5].clone()]);
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection("c").unwrap();
    let mut scan = h.scan(ScanOptions { after: Some(c), ..ScanOptions::default() }).unwrap();
    let got: Vec<_> = drain_scan(&mut scan).await.into_iter().map(|i| i.located.entry).collect();
    assert_eq!(ids(&got), id_strs([3, 4, 6]));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn scan_cancelled_page_is_not_lost() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    h.put((1..=20).map(|i| record(i, 50)).collect()).await.unwrap();
    let mut scan = h.scan(ScanOptions { page_records: 5, ..ScanOptions::default() }).unwrap();
    let mut got = Vec::new();
    let mut dropped = 0;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let page = if attempts <= 5 {
            tokio::select! {
                biased;
                () = tokio::task::yield_now() => {
                    dropped += 1;
                    continue;
                }
                r = scan.next_page() => r.unwrap(),
            }
        } else {
            scan.next_page().await.unwrap()
        };
        let Some(p) = page else { break };
        got.extend(p.items.into_iter().map(|i| i.located.entry));
    }
    assert!(dropped >= 1);
    assert_eq!(got, live_entries(&h).await);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn scan_snapshot_is_a_prefix() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    h.put((1..=5).map(|i| record(i, 50)).collect()).await.unwrap();
    let opts = ScanOptions { page_records: 2, ..ScanOptions::default() };
    let mut scan = h.scan(opts).unwrap();
    let mut n = scan.next_page().await.unwrap().unwrap().items.len();
    h.put((6..=10).map(|i| record(i, 50)).collect()).await.unwrap();
    n += drain_scan(&mut scan).await.len();
    assert_eq!(n, 6);
    let mut scan = h.scan(opts).unwrap();
    assert_eq!(drain_scan(&mut scan).await.len(), 11);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn scan_bad_cursors() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    h.put((1..=5).map(|i| record(i, 50)).collect()).await.unwrap();
    for t in ["x", "1", "01.5", "1.-5", "1.05", ""] {
        let e = ScanCursor::from_token(t).unwrap_err();
        assert!(matches!(e, Error::InvalidQuery(_)), "{t}: {e:?}");
    }
    let mut scan = h.scan(ScanOptions { page_records: 2, ..ScanOptions::default() }).unwrap();
    let valid = scan.next_page().await.unwrap().unwrap().next.to_token();
    let (gen_s, off_s) = valid.split_once('.').unwrap();
    let (generation, off): (u32, u64) = (gen_s.parse().unwrap(), off_s.parse().unwrap());
    let cdx_len = h.stats().cdx_len;
    let next_err = |t: String| {
        let h = h.clone();
        async move {
            let c = ScanCursor::from_token(&t).unwrap();
            let mut s = h.scan(ScanOptions { after: Some(c), ..ScanOptions::default() }).unwrap();
            s.next_page().await.err().unwrap()
        }
    };
    let e = next_err(format!("{}.{off}", generation + 1)).await;
    assert!(matches!(e, Error::StaleLocator { .. }), "{e:?}");
    let e = next_err(format!("{generation}.{}", cdx_len + 10)).await;
    assert!(matches!(e, Error::InvalidQuery(_)), "{e:?}");
    let e = next_err(format!("{generation}.{}", off + 1)).await;
    assert!(matches!(e, Error::InvalidQuery(_)), "{e:?}");
    for o in [
        ScanOptions { page_records: 0, ..ScanOptions::default() },
        ScanOptions { page_records: MAX_SCAN_PAGE + 1, ..ScanOptions::default() },
        ScanOptions { page_bytes: 0, ..ScanOptions::default() },
    ] {
        assert!(matches!(h.scan(o).err().unwrap(), Error::InvalidQuery(_)));
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn scan_header_only_mismatch_item_error() {
    let (_td, repo, h) = fresh(RawCheck::HeaderOnly, Codec::None, &Config::default()).await;
    let bad_payload = record(1, 100).as_bytes().to_vec();
    let mut bad_meta = meta_for(&record(1, 100), &bad_payload, true);
    bad_meta.record_id = id_str(2).into();
    let items = vec![plain_item(3, 100), (bad_meta, bad_payload), plain_item(4, 100)];
    h.put_raw(items.into_iter().map(|(meta, payload)| RawItem { meta, payload }).collect())
        .await
        .unwrap();
    let mut scan = h.scan(ScanOptions { fetch: Fetch::Decoded, ..ScanOptions::default() }).unwrap();
    let items = drain_scan(&mut scan).await;
    assert_eq!(items.len(), 4);
    let flags: Vec<bool> = items.iter().map(|i| matches!(i.record, Some(Ok(_)))).collect();
    assert_eq!(flags, [true, true, false, true]);
    assert!(matches!(items[2].record, Some(Err(Error::NotDecodable { .. }))));
    let mut scan = h.scan(ScanOptions { fetch: Fetch::Raw, ..ScanOptions::default() }).unwrap();
    assert!(drain_scan(&mut scan).await.iter().all(|i| matches!(i.record, Some(Ok(_)))));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn scan_after_shutdown() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let mut scan = h.scan(ScanOptions::default()).unwrap();
    repo.shutdown().await.unwrap();
    let e = scan.next_page().await.err().unwrap();
    assert!(matches!(e, Error::ShuttingDown), "{e:?}");
    assert!(matches!(h.scan(ScanOptions::default()).err().unwrap(), Error::ShuttingDown));
}
}

// ---- Search.

fn dated(i: usize) -> String {
    format!("2026-11-13T10:{:02}:{:02}Z", i / 60, i % 60)
}

rt_test! {
async fn search_host_port_path_filters() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let urls = [
        "http://example.org/a",
        "http://www.example.org/a",
        "http://sub.example.org/b/c",
        "http://example.org:8080/a",
        "https://example.org/b?x=1",
        "https://other.org/a",
        "http://notexample.org/a",
    ];
    let recs: Vec<Record> = urls
        .iter()
        .enumerate()
        .map(|(i, u)| record_at(i + 1, "response", u, DATE))
        .collect();
    h.put(recs).await.unwrap();
    repo.sync_index().await.unwrap();
    let set = |n: &[usize]| id_strs(n.iter().copied()).into_iter().collect::<BTreeSet<_>>();
    use HostMatch::{Exact, WithSubdomains};
    use PathFilter as P;
    use PortFilter as Pt;
    let cases = [
        (pattern("example.org", Exact, Pt::Any, P::Any), set(&[1, 4, 5])),
        (pattern("example.org", WithSubdomains, Pt::Any, P::Any), set(&[1, 2, 3, 4, 5])),
        (pattern("example.org", WithSubdomains, Pt::Explicit(8080), P::Any), set(&[4])),
        (pattern("example.org", WithSubdomains, Pt::Default, P::Any), set(&[1, 2, 3, 5])),
        (pattern("example.org", WithSubdomains, Pt::Any, P::Prefix("/b")), set(&[3, 5])),
        (pattern("example.org", Exact, Pt::Any, P::Exact("/a")), set(&[1, 4])),
        (pattern("example.org", Exact, Pt::Default, P::Exact("/a")), set(&[1])),
    ];
    for (i, (p, want)) in cases.iter().enumerate() {
        assert_eq!(&search_ids(&h, p).await, want, "case {i}");
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn search_pagination_is_stable() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let recs: Vec<Record> = (1..=25)
        .map(|i| record_at(i, "response", &format!("http://example.org/p{}", i % 20), &dated(i)))
        .collect();
    h.put(recs).await.unwrap();
    repo.sync_index().await.unwrap();
    let p = pattern("example.org", HostMatch::WithSubdomains, PortFilter::Any, PathFilter::Any);
    let whole = h.search(&p, &SearchQuery::default()).await.unwrap();
    assert!(whole.next.is_none());
    assert_eq!(whole.hits.len(), 25);
    let mut got = Vec::new();
    let mut after = None;
    loop {
        let page = h.search(&p, &SearchQuery { limit: 4, after }).await.unwrap();
        assert!(page.hits.len() <= 4);
        got.extend(page.hits);
        match page.next {
            Some(c) => after = Some(SearchCursor::from_token(&c.to_token()).unwrap()),
            None => break,
        }
    }
    assert_eq!(got, whole.hits);
    let uniq: BTreeSet<_> = got.iter().map(|l| (l.entry.segment, l.entry.offset)).collect();
    assert_eq!(uniq.len(), 25);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn search_validation() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let p = pattern("example.org", HostMatch::Exact, PortFilter::Any, PathFilter::Any);
    for limit in [0, 10_001] {
        let e = h.search(&p, &SearchQuery { limit, after: None }).await.unwrap_err();
        assert!(matches!(e, Error::InvalidQuery(_)), "{e:?}");
    }
    let e = SearchCursor::from_token("garbage").unwrap_err();
    assert!(matches!(e, Error::InvalidQuery(_)), "{e:?}");
    assert!(UrlPattern::new("", HostMatch::Exact, PortFilter::Any, PathFilter::Any).is_err());
    repo.shutdown().await.unwrap();
}
}

// ---- Lookups.

rt_test! {
async fn get_by_id_most_recent_copy() {
    let (_td, repo, a) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let b = repo.create_collection("b", Codec::None).await.unwrap();
    let body_a = record(7, 50);
    let body_b = record(7, 80);
    a.put(vec![body_a]).await.unwrap();
    let second = a.put(vec![body_b.clone()]).await.unwrap().remove(0);
    b.put(vec![record_at(7, "response", "https://example.org/7", "2027-01-01T00:00:00Z")])
        .await
        .unwrap();
    repo.sync_index().await.unwrap();
    assert_eq!(repo.find_by_id(&id_str(7), Some(a.id())).await.unwrap(), Some(second.clone()));
    let (at, bytes) = repo.get_by_id(&id_str(7), Some(a.id())).await.unwrap().unwrap();
    assert_eq!((at, bytes), (second, body_b.as_bytes().to_vec()));
    let any = repo.find_by_id(&id_str(7), None).await.unwrap().unwrap();
    assert_eq!(any.collection, b.id());
    assert!(repo.find_by_id(&id_str(8), None).await.unwrap().is_none());
    assert!(repo.get_by_id(&id_str(8), None).await.unwrap().is_none());
    let e = repo
        .find_by_id(&id_str(7), Some(CollectionId::from_u128(1)))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::UnknownCollection(_)), "{e:?}");
    for bad in [String::new(), "x".repeat(300 * 1024)] {
        let e = repo.find_by_id(&bad, None).await.unwrap_err();
        assert!(matches!(e, Error::InvalidQuery(_)), "{e:?}");
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn get_by_url_nearest_date() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    let u = "http://example.org/x";
    let recs = vec![
        record_at(1, "response", u, "2020-01-01T00:00:00Z"),
        record_at(2, "response", u, "2022-01-01T00:00:00Z"),
        record_at(3, "response", u, "2024-01-01T00:00:00Z"),
        record_at(4, "request", u, "2023-01-01T00:00:00Z"),
        record_at(5, "response", "dns:example.org", "2024-01-01T00:00:00Z"),
    ];
    h.put(recs.clone()).await.unwrap();
    repo.sync_index().await.unwrap();
    let d = |s: &str| Some(WarcDate::parse(s).unwrap());
    let id_of = |r: Option<(Located, Vec<u8>)>| r.map(|(l, _)| l.entry.record_id.to_string());
    let got = h.get_by_url(u, d("2023-02-01T00:00:00Z")).await.unwrap();
    assert_eq!(got.as_ref().unwrap().1, recs[2].as_bytes());
    assert_eq!(id_of(got), Some(id_str(3)));
    let got = h.get_by_url(u, d("2021-12-01T00:00:00Z")).await.unwrap();
    assert_eq!(id_of(got), Some(id_str(2)));
    let got = h.get_by_url(u, None).await.unwrap();
    assert_eq!(id_of(got), Some(id_str(3)));
    let got = h.get_by_url("HTTP://Example.org:80/x", d("2021-12-01T00:00:00Z")).await.unwrap();
    assert_eq!(id_of(got), Some(id_str(2)));
    let got = h.get_by_url("dns:example.org", None).await.unwrap();
    assert_eq!(got.unwrap().1, recs[4].as_bytes());
    assert!(h.find_by_url("dns:EXAMPLE.org", None).await.unwrap().is_none());
    let e = h.find_by_url("", None).await.unwrap_err();
    assert!(matches!(e, Error::InvalidQuery(_)), "{e:?}");
    repo.shutdown().await.unwrap();
}
}

type Observed = (
    BTreeSet<String>,
    Option<Located>,
    Option<(Located, Vec<u8>)>,
    usize,
    Vec<Entry<'static>>,
);

async fn observe(repo: &Repository, h: &CollectionHandle) -> Observed {
    let p = pattern(
        "example.org",
        HostMatch::WithSubdomains,
        PortFilter::Any,
        PathFilter::Any,
    );
    let hits = search_ids(h, &p).await;
    let by_id = repo.find_by_id(&id_str(3), Some(h.id())).await.unwrap();
    let by_url = h.get_by_url(&url(4), None).await.unwrap();
    let count = repo
        .get_by_id(&id_str(5), None)
        .await
        .unwrap()
        .map_or(0, |(_, b)| b.len());
    let mut scan = h.scan(ScanOptions::default()).unwrap();
    let scanned = drain_scan(&mut scan)
        .await
        .into_iter()
        .map(|i| i.located.entry)
        .collect();
    (hits, by_id, by_url, count, scanned)
}

rt_test! {
async fn queries_after_restart() {
    let (td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    h.put((1..=5).map(|i| record(i, 60)).collect()).await.unwrap();
    h.put((6..=10).map(|i| record(i, 60)).collect()).await.unwrap();
    repo.sync_index().await.unwrap();
    let before = observe(&repo, &h).await;
    assert_eq!((before.0.len(), before.4.len()), (10, 11));
    assert!(before.1.is_some() && before.2.is_some() && before.3 > 0);
    repo.shutdown().await.unwrap();
    drop((repo, h));
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection("c").unwrap();
    repo.sync_index().await.unwrap();
    assert_eq!(observe(&repo, &h).await, before);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn queries_eventually_consistent() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    repo.sync_index().await.unwrap();
    let sem = Arc::new(tokio::sync::Semaphore::new(0));
    *index_hooks(&repo).hold.lock().unwrap() = Some(Arc::clone(&sem));
    h.put(vec![record(1, 40)]).await.unwrap();
    let mut scan = h.scan(ScanOptions::default()).unwrap();
    assert_eq!(drain_scan(&mut scan).await.len(), 2);
    *index_hooks(&repo).hold.lock().unwrap() = None;
    sem.add_permits(16);
    repo.sync_index().await.unwrap();
    assert!(repo.find_by_id(&id_str(1), None).await.unwrap().is_some());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn queries_after_shutdown() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &Config::default()).await;
    repo.shutdown().await.unwrap();
    let e = repo.find_by_id("x", None).await.unwrap_err();
    assert!(matches!(e, Error::ShuttingDown), "{e:?}");
    let p = pattern("example.org", HostMatch::Exact, PortFilter::Any, PathFilter::Any);
    let e = h.search(&p, &SearchQuery::default()).await.unwrap_err();
    assert!(matches!(e, Error::ShuttingDown), "{e:?}");
    let e = h.find_by_url("http://example.org/", None).await.unwrap_err();
    assert!(matches!(e, Error::ShuttingDown), "{e:?}");
}
}

rt_test! {
async fn concurrent_queries_while_putting() {
    let (_td, repo, h) = fresh(RawCheck::Full, Codec::None, &small_config()).await;
    h.put((1..=10).map(|i| record(i, 50)).collect()).await.unwrap();
    repo.sync_index().await.unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let mut putters = Vec::new();
    for t in 0..4usize {
        let h = h.clone();
        putters.push(tokio::spawn(async move {
            for j in 0..30 {
                h.put(vec![record(100 + t * 100 + j, 50)]).await.unwrap();
            }
        }));
    }
    let mut queriers = Vec::new();
    for _ in 0..4 {
        let (h, repo, done) = (h.clone(), repo.clone(), Arc::clone(&done));
        queriers.push(tokio::spawn(async move {
            let p = pattern("example.org", HostMatch::WithSubdomains, PortFilter::Any, PathFilter::Any);
            loop {
                let finished = done.load(Ordering::Acquire);
                let mut scan = h.scan(ScanOptions { page_records: 50, ..ScanOptions::default() }).unwrap();
                let items = drain_scan(&mut scan).await;
                let uniq: BTreeSet<_> = items.iter().map(|i| i.located.entry.offset).collect();
                assert_eq!(uniq.len(), items.len());
                let mut after = None;
                loop {
                    let page = h.search(&p, &SearchQuery { limit: 50, after }).await.unwrap();
                    after = page.next;
                    if after.is_none() {
                        break;
                    }
                }
                for i in [1usize, 5, 10] {
                    let (at, bytes) = repo.get_by_id(&id_str(i), None).await.unwrap().unwrap();
                    assert_eq!(bytes, record(i, 50).as_bytes());
                    assert_eq!(at.entry.record_id.as_ref(), id_str(i));
                }
                if finished {
                    break;
                }
            }
        }));
    }
    for p in putters {
        p.await.unwrap();
    }
    done.store(true, Ordering::Release);
    for q in queriers {
        q.await.unwrap();
    }
    repo.sync_index().await.unwrap();
    let p = pattern("example.org", HostMatch::WithSubdomains, PortFilter::Any, PathFilter::Any);
    let mut total = 0;
    let mut after = None;
    loop {
        let page = h.search(&p, &SearchQuery { limit: 40, after }).await.unwrap();
        total += page.hits.len();
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(total, 130);
    for t in 0..4usize {
        for j in 0..30 {
            assert!(repo.find_by_id(&id_str(100 + t * 100 + j), None).await.unwrap().is_some());
        }
    }
    repo.shutdown().await.unwrap();
}
}

// ---- Error kinds.

#[test]
fn error_kinds_m19() {
    assert_eq!(Error::InvalidQuery("x").kind(), ErrorKind::Invalid);
    let t = Error::Envelope(EnvelopeError::Truncated { offset: 0 });
    assert_eq!(t.kind(), ErrorKind::Invalid);
    let io = Error::Envelope(EnvelopeError::Io(std::io::Error::other("x")));
    assert_eq!(io.kind(), ErrorKind::Io);
    let imp = Error::Import {
        report: Box::default(),
        source: Box::new(Error::ShuttingDown),
    };
    assert_eq!(imp.kind(), ErrorKind::Unavailable);
    let c = Error::IndexQuery(masstuffy_index::Error::Corrupt("x".into()));
    assert_eq!(c.kind(), ErrorKind::Corrupt);
    let l = Error::IndexQuery(masstuffy_index::Error::InvalidCursor);
    assert_eq!(l.kind(), ErrorKind::Io);
}
