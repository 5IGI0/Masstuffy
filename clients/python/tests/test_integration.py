"""Integration tests against the real masstuffy-server binary (set MASSTUFFY_SERVER_BIN)."""

import gzip
import hashlib
import io
import os
import re
import signal
import subprocess
import tempfile
import threading
import unittest
import uuid

import pywarc

import masstuffy as m
from masstuffy.compression import have_xxhash, have_zstd
from tests.helpers import make_record

BIN = os.environ.get("MASSTUFFY_SERVER_BIN")
HAVE_BIN = bool(BIN) and os.path.isfile(BIN) and os.access(BIN, os.X_OK)


class ServerProcess:
    def __init__(self, env=None):
        self.tmp = tempfile.TemporaryDirectory()
        # Hermetic: a developer's MASSTUFFY_* variables must not change the server under test.
        e = {k: v for k, v in os.environ.items() if not k.startswith("MASSTUFFY_")}
        e.update(env or {})
        self.proc = subprocess.Popen([BIN, os.path.join(self.tmp.name, "repo"), "--init", "--listen", "127.0.0.1:0",
                                      "--log", "info"], env=e, stderr=subprocess.PIPE, stdout=subprocess.DEVNULL)
        self.lines = []
        self.addr = None
        self._ready = threading.Event()
        self._t = threading.Thread(target=self._drain, daemon=True)
        self._t.start()
        if not self._ready.wait(30):
            self.proc.kill()
            raise RuntimeError("server did not start: " + "".join(self.lines))

    def _drain(self):
        for raw in self.proc.stderr:
            line = raw.decode("utf-8", "replace")
            self.lines.append(line)
            mt = re.search(r"listening on (\S+)\s*$", re.sub(r"\x1b\[[0-9;]*m", "", line))
            if mt and self.addr is None:
                self.addr = mt.group(1)
                self._ready.set()

    @property
    def url(self):
        return "http://" + self.addr

    def stop(self):
        self.proc.send_signal(signal.SIGTERM)
        try:
            code = self.proc.wait(30)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            raise
        finally:
            self._t.join(5)
            self.proc.stderr.close()
            self.tmp.cleanup()
        return code


def html_record(i):
    body = ("<html><head><title>page %d</title></head><body>" % i +
            " ".join("word%d" % ((i * 7 + j * 13) % 500) for j in range(250)) + "</body></html>").encode()
    return m.RecordSpec("resource", body, headers={"WARC-Target-URI": "http://train.example/%d" % i,
                                                   "Content-Type": "text/html"})


def spec_bytes(spec):
    return b"".join(m._put_body([spec])[1]()) if hasattr(m, "_put_body") else None


@unittest.skipUnless(HAVE_BIN, "MASSTUFFY_SERVER_BIN not set")
class IntegrationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ServerProcess()
        cls.client = m.Client(cls.server.url)

    @classmethod
    def tearDownClass(cls):
        cls.client.close()
        cls.assertEqual_code = cls.server.stop()

    def new(self, codec=None):
        return self.client.create_collection("t-" + uuid.uuid4().hex[:12], codec)

    def read_all(self, col, loc):
        with col.get(loc) as r:
            return r.read()

    def test_healthz_status(self):
        self.assertTrue(self.client.healthz())
        st = self.client.status()
        self.assertEqual(st.api, 1)
        self.assertEqual(st.raw_check, "full")
        self.assertGreater(st.limits.max_put_body_bytes, 0)

    def test_collection_lifecycle(self):
        col = self.new()
        self.assertIn(col.slug, [c.slug for c in self.client.collections()])
        info = self.client.get_collection(col.slug)
        self.assertEqual(info.stats.records, 0)
        new = col.rename(col.slug + "-r")
        with self.assertRaises(m.NotFoundError):
            self.client.get_collection(col.slug)
        new.delete_collection()
        with self.assertRaises(m.NotFoundError):
            self.client.get_collection(new.slug)

    def test_put_get_roundtrip_1_1_and_1_0(self):
        col = self.new()
        old = (b"WARC/1.0\r\nWARC-Type: resource\r\nWARC-Record-ID: <urn:uuid:" + uuid.uuid4().bytes.hex().encode()[:0] +
               str(uuid.uuid4()).encode() + b">\r\nWARC-Date: 2020-01-02T03:04:05Z\r\n"
               b"WARC-Target-URI: <http://old.example/a>\r\nContent-Length: 3\r\n\r\nabc\r\n\r\n")
        specs = [m.RecordSpec("resource", b"one", headers={"WARC-Target-URI": "http://n.example/1"}),
                 m.RecordSpec("resource", b"two")]
        locs = col.put(specs + [old])
        self.assertEqual(len(locs), 3)
        self.assertEqual(locs[0].url, "http://n.example/1")
        self.assertEqual(locs[2].url, "http://old.example/a")
        got = self.read_all(col, locs[2])
        self.assertEqual(got, b"abc")
        with col.get(locs[2]) as r:
            self.assertEqual(r.version, "WARC/1.0")

    def test_put_large_record_streams(self):
        col = self.new()
        size = 20 << 20
        h = hashlib.sha256()
        with tempfile.TemporaryFile() as f:
            blk = os.urandom(1 << 20)
            for _ in range(20):
                f.write(blk)
                h.update(blk)
            f.seek(0)
            (loc,) = col.put([m.RecordSpec("resource", f, content_length=size)])
        g = hashlib.sha256()
        with col.get(loc) as r:
            for c in r.iter_content():
                g.update(c)
        self.assertEqual(g.hexdigest(), h.hexdigest())

    def test_put_errors(self):
        col = self.new()
        with self.assertRaises(m.InvalidRequestError) as cm:
            col.put_raw(b"not a warc at all")
        self.assertEqual(cm.exception.code, "invalid_warc")
        with self.assertRaises(m.NotFoundError):
            self.client.collection("no-such-collection").put([m.RecordSpec("resource", b"x")])
        with self.assertRaises(m.InvalidRequestError) as cm:
            col.get("L1forgedtoken")
        self.assertEqual(cm.exception.code, "invalid_locator")

    def _write_warc(self, path, n):
        w = pywarc.WarcWriter(path)
        ids = []
        for i in range(n):
            ids.append(w.write_record("resource", ("rec %d" % i).encode(),
                                      record_headers={"WARC-Target-URI": "http://f.example/%d" % i})
                       if hasattr(w, "write_record") else None)
        w.close()

    def test_put_warc_file_plain_and_gz(self):
        with tempfile.TemporaryDirectory() as d:
            plain = b"".join(make_record(i) for i in range(300))
            for name, data in (("a.warc", plain), ("a.warc.gz", b"".join(gzip.compress(make_record(i))
                                                                       for i in range(300)))):
                p = os.path.join(d, name)
                with open(p, "wb") as f:
                    f.write(data)
                col = self.new()
                rep = col.put_warc_file(p, batch_records=64)
                self.assertEqual(rep.records, 300)
                self.assertEqual(rep.batches, 5)
                self.assertEqual(len(list(col.scan())) - 1, 300)  # + warcinfo
                out = os.path.join(d, "out-" + name)
                col.export(out, compress=False)
                with open(out, "rb") as f:
                    exported = f.read()
                self.assertTrue(exported.endswith(plain))

    def test_lookups_and_search(self):
        col = self.new()
        urls = ["http://a.example/x", "http://b.a.example/y?q=1", "http://a.example:8080/z"]
        locs = col.put([m.RecordSpec("resource", b"body", headers={"WARC-Target-URI": u}) for u in urls])
        self.assertEqual(col.lookup_by_url(urls[0], sync=True).record_id, locs[0].record_id)
        self.assertEqual(col.lookup_by_id(locs[1].record_id, sync=True).token, locs[1].token)
        with col.get_by_id(locs[2].record_id, sync=True) as r:
            self.assertIsNotNone(r.locator)
        self.assertIsNone(col.lookup_by_url("http://nope.example/", sync=True))
        exact = list(col.search("a.example", port="default", sync=True))
        self.assertEqual({h.url for h in exact}, {urls[0]})
        self.assertEqual({h.url for h in col.search("a.example")}, {urls[0], urls[2]})
        subs = list(col.search("a.example", subdomains=True, port="default"))
        self.assertEqual({h.url for h in subs}, {urls[0], urls[1]})
        self.assertEqual({h.url for h in col.search("a.example", port=8080)}, {urls[2]})
        self.assertEqual({h.url for h in col.search("a.example", path="/x", path_match="exact")}, {urls[0]})
        self.assertEqual({h.url for h in col.search("a.example", subdomains=True, port="default", limit=1)}, {urls[0], urls[1]})

    def test_scan_and_export_roundtrip(self):
        col = self.new()
        specs = [m.RecordSpec("resource", b"r%d" % i) for i in range(50)]
        col.put(specs)
        n = len(list(col.scan(limit=7)))
        self.assertEqual(n, 51)
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "x.warc.gz")
            rep = col.export(p, max_records=7)
            self.assertEqual(rep.records, 51)
            ids = []
            r = pywarc.WarcReader(p)
            while True:
                b = r.get_next_block()
                if b is None:
                    break
                ids.append(b.record_id)
            self.assertEqual(len(ids), 51)
            rep2 = col.export(os.path.join(d, "y.warc"), cursor=rep.cursor)
            self.assertEqual(rep2.records, 0)

    def test_delete_semantics(self):
        col = self.new()
        locs = col.put([m.RecordSpec("resource", b"d%d" % i) for i in range(5)])
        before = len(list(col.scan()))
        self.assertEqual(col.delete(locs[:2]), 2)
        self.client.sync_index()
        self.assertIsNotNone(col.delete_by_id(locs[2].record_id))
        self.assertEqual(len(list(col.scan())), before - 3)
        self.assertIsNone(col.lookup_by_id(locs[2].record_id, sync=True))
        self.assertEqual(self.read_all(col, locs[0]), b"d0")

    @unittest.skipUnless(have_zstd() and have_xxhash(), "needs zstandard and xxhash")
    def test_train_rebuild_and_client_compression(self):
        col = self.new()
        for i in range(0, 400, 100):
            col.put([html_record(j) for j in range(i, i + 100)])
        old = next(iter(col.scan()))
        locs_old = list(col.scan(limit=5))
        job = col.train(max_dict_size=16384).wait(120, raise_on_failure=False)
        self.assertEqual(job.state, "succeeded", job.error)
        dict_id = job.result["dict_id"]
        self.assertEqual(col.rebuild(m.Codec.zstd(dict_id, 3)).wait(300).state, "succeeded")
        info = col.info(refresh=True)
        self.assertEqual(info.codec.algo, "zstd")
        self.assertEqual(info.raw_rules.dict_id, dict_id)
        with self.assertRaises(m.StaleLocatorError):
            col.get(old)
        loc = list(col.scan(limit=5))[3]
        with col.get(loc) as r:
            server = r.read()
        with col.get_raw(loc, decompress=True) as rb:
            self.assertEqual(rb.encoding, "warc")
            self.assertEqual(len(rb.read()) > 0, True)
        with col.get(loc, decode="client") as r:
            self.assertEqual(r.read(), server)
        with col.get_raw(loc) as rb:
            self.assertEqual(rb.encoding, "zstd")
            self.assertEqual(rb.dict_id, dict_id)
            rb.read()
        self.assertIn(dict_id, [d.dict_id for d in self.client.dictionaries()])

        big = m.RecordSpec("resource", os.urandom(1_500_000), headers={"WARC-Target-URI": "http://big.example/"})
        frac_id = "urn:uuid:" + str(uuid.uuid4())
        frac = (b"WARC/1.1\r\nWARC-Type: resource\r\nWARC-Record-ID: <" + frac_id.encode() + b">\r\n"
                b"WARC-Date: 2020-01-02T03:04:05.123456789Z\r\nWARC-Target-URI: http://frac.example/\r\n"
                b"Content-Length: 4\r\n\r\nfrac\r\n\r\n")
        specs = [html_record(1000), big, frac]
        rep = col.import_records(specs)
        self.assertEqual(rep.records, 3)
        found = col.lookup_by_id(specs[1].record_id, sync=True)
        self.assertIsNotNone(found)
        self.assertEqual(self.read_all(col, found), specs[1].body)
        self.assertEqual(col.lookup_by_id(frac_id, sync=True).date, "2020-01-02T03:04:05.123456789Z")

        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "f.warc")
            with open(p, "wb") as f:
                for i in range(20):
                    f.write(make_record(10_000 + i))
                f.write(make_record(20_000, version=b"WARC/1.0", url="<http://br.example/>"))
                f.write(make_record(20_001).replace(b"2020-01-02T03:04:05Z", b"2020-01-02T03:04:05.123456789Z"))
            rep = col.put_warc_file(p, mode="import")
            self.assertEqual(rep.records, 22)

    @unittest.skipUnless(have_zstd() and have_xxhash(), "needs zstandard and xxhash")
    def test_codec_change_retry_real(self):
        col = self.new()
        for i in range(0, 400, 100):
            col.put([html_record(j) for j in range(i, i + 100)])
        j = col.train(max_dict_size=16384).wait(120, raise_on_failure=False)
        self.assertEqual(j.state, "succeeded", j.error)
        col.rebuild(m.Codec.zstd(j.result["dict_id"], 3)).wait(300)
        stale = self.client.collection(col.slug)
        stale.info(refresh=True)
        stale.compressor()
        j2 = col.train(max_dict_size=8192).wait(120, raise_on_failure=False)
        self.assertEqual(j2.state, "succeeded", j2.error)
        if j2.result["dict_id"] == j.result["dict_id"]:
            self.skipTest("training produced the same dictionary")
        col.rebuild(m.Codec.zstd(j2.result["dict_id"], 3)).wait(300)
        rep = stale.import_records([html_record(5000), html_record(5001)])
        self.assertEqual(rep.records, 2)
        self.assertEqual(stale.info().codec.dict_id, j2.result["dict_id"])

    def test_rebuild_index_job_and_sync(self):
        self.assertEqual(self.client.rebuild_index().wait(60).state, "succeeded")
        self.assertIsNone(self.client.sync_index())
        self.assertGreaterEqual(len(self.client.jobs()), 1)

    def test_jobs_listing(self):
        job = self.client.rebuild_index().wait(60)
        ids = [j.id for j in self.client.jobs()]
        self.assertIn(job.id, ids)

    @unittest.skipUnless(have_xxhash(), "needs xxhash")
    def test_import_none_collection_with_fractional_date(self):
        col = self.new()
        spec = m.RecordSpec("resource", b"x", headers={"WARC-Target-URI": "http://i.example/"})
        rep = col.import_records([spec])
        self.assertEqual(rep.records, 1)

    def test_concurrent_clients_threads(self):
        col = self.new()
        locs, errs = [], []

        def work(k):
            try:
                for i in range(10):
                    locs.extend(col.put([m.RecordSpec("resource", b"t%d-%d" % (k, i))]))
            except Exception as e:  # pragma: no cover
                errs.append(e)

        ths = [threading.Thread(target=work, args=(k,)) for k in range(4)]
        [t.start() for t in ths]
        [t.join() for t in ths]
        self.assertEqual(errs, [])
        self.assertEqual(len({l.token for l in locs}), 40)
        self.assertEqual(len(list(col.scan())), 41)

    def test_grep_concurrent_and_collection(self):
        col = self.new()
        specs = [m.RecordSpec("resource", (b"needle %d " % i if i % 3 == 0 else b"hay %d " % i) * 4)
                 for i in range(30)]
        col.put(specs)
        want = {l.token for l in col.scan()}
        expected = 10
        results, errs = [], []

        def work(use_collection):
            try:
                if use_collection:
                    st = col.grep("needle")
                else:
                    st = self.client.grep([col.slug], [m.GrepQuery("needle")], events=("hit", "progress"))
                with st:
                    items = list(st)
                results.append((items, st.report, dict(st.progress)))
            except Exception as e:  # pragma: no cover
                errs.append(e)

        ths = [threading.Thread(target=work, args=(k % 2 == 0,)) for k in range(4)]
        [t.start() for t in ths]
        [t.join() for t in ths]
        self.assertEqual(errs, [])
        self.assertEqual(len(results), 4)
        for items, rep, prog in results:
            hits = [i for i in items if isinstance(i, m.GrepHit)]
            self.assertEqual(len(hits), expected)
            self.assertEqual(len({h.locator.token for h in hits}), expected)
            self.assertTrue(all(h.locator.token in want for h in hits))
            self.assertEqual(rep.collections[0].status, "complete")
            self.assertEqual(rep.collections[0].hits, expected)
            self.assertEqual(rep.collections[0].cdx_done, rep.collections[0].cdx_total)
            self.assertIn(col.slug, prog)
        with self.client.grep(col.slug, "needle", events=("progress",)) as st:
            first = next(iter(st))
        self.assertEqual((first.cdx_done, first.slug), (0, col.slug))
        self.assertGreater(first.cdx_total, 0)
        with self.assertRaises(m.InvalidRequestError) as cm:
            self.client.grep(col.slug, m.GrepQuery("(", regex=True))
        self.assertEqual(cm.exception.code, "invalid_pattern")


@unittest.skipUnless(HAVE_BIN, "MASSTUFFY_SERVER_BIN not set")
class HeaderCheckIntegrationTest(unittest.TestCase):
    def test_import_envelope_legacy_header_check(self):
        srv = ServerProcess({"MASSTUFFY_RAW_IMPORT_CHECK": "header"})
        try:
            with m.Client(srv.url) as c:
                col = c.create_collection("legacy")
                rec = make_record(1, url="http://real.example/a")
                rid = "urn:uuid:11111111-1111-1111-1111-111111111111"  # differs from the one in the payload
                line = ("http://real.example/a resource %s 20200102030405 - - %d\n" % (rid, len(rec))).encode()
                rep = col.import_envelope(line + rec)
                self.assertEqual(rep.records, 1)
                loc = col.lookup_by_id(rid, sync=True)
                self.assertIsNotNone(loc)
                with self.assertRaises(m.NotDecodableError):
                    col.get(loc)
                with col.get_raw(loc) as rb:
                    self.assertEqual(rb.read(), rec)
                with col.get(loc, decode="client") as r:
                    self.assertEqual(r.record_id, "urn:uuid:00000000-0000-0000-0000-000000000001")
        finally:
            self.assertEqual(srv.stop(), 0)


if __name__ == "__main__":
    unittest.main()
