import gzip
import io
import json
import os
import tempfile
import unittest
import zlib

import pywarc

import masstuffy as m
from masstuffy.compression import have_xxhash, have_zstd
from masstuffy.models import Locator
from tests.fakeserver import Drop, FakeServer, Resp, error_resp, json_resp
from tests.helpers import make_record, make_warc, parse_v1

RULES_NONE = {"payload": "warc", "single_frame": True, "dict_id": None, "dictless_min_content_size": 1048577}
LIMITS = {"max_put_body_bytes": 64 << 20, "max_put_records": 10000, "max_import_body_bytes": None,
          "max_json_body_bytes": 1, "max_export_page_bytes": 1, "request_timeout_ms": 1,
          "body_idle_timeout_ms": 1, "max_concurrent_requests": 1}


def status_obj(**limits):
    return {"version": "0", "api": 1, "repository": "r", "raw_check": "full", "max_record_bytes": 1 << 30,
            "collections": 0, "dictionaries": 0, "jobs_running": 0, "limits": dict(LIMITS, **limits),
            "open_report": None}


def col_obj(slug="c", codec=None, rules=None, stats=False, gen=0):
    o = {"id": "uuid-" + slug, "slug": slug, "generation": gen, "codec": codec or {"algo": "none"},
         "raw_rules": rules or RULES_NONE}
    if stats:
        o["stats"] = {"commits": 1, "records": 2, "cdx_len": 3}
    return o


def loc_obj(i, rid=None):
    return {"locator": "L1tok%d" % i, "collection": "u", "generation": 0, "segment": 1, "offset": i, "raw_size": 5,
            "digest": "0" * 16, "date": "2020-01-02T03:04:05Z", "warc_type": "resource",
            "record_id": rid or "urn:uuid:%d" % i, "url": None}


def count_records(data):
    r = pywarc.WarcReader(io.BytesIO(data), compressed=False)
    n = 0
    while r.get_next_block() is not None:
        n += 1
    return n


def put_responder(req):
    n = count_records(req.body)
    return json_resp(201, {"records": [loc_obj(i) for i in range(n)]})


class Base(unittest.TestCase):
    def setUp(self):
        self.srv = FakeServer().start()
        self.addCleanup(self.srv.stop)
        self.client = m.Client(self.srv.url, retries=0, backoff=0.01)
        self.addCleanup(self.client.close)
        self.srv.route("GET", "/v1/status", json_resp(200, status_obj()))
        self.srv.route("GET", "/v1/collections/c", json_resp(200, col_obj()))
        self.col = self.client.collection("c")

    def reqs(self, method=None, prefix=""):
        return [r for r in self.srv.requests if (method is None or r.method == method) and r.path.startswith(prefix)]


class CollectionsTest(Base):
    def test_collections_crud_requests(self):
        self.srv.route("POST", "/v1/collections", json_resp(201, col_obj("new", stats=True)))
        self.srv.route("GET", "/v1/collections", json_resp(200, {"collections": [col_obj("a"), col_obj("b")]}))
        self.srv.route("PATCH", "/v1/collections/c", json_resp(200, col_obj("d", stats=True)))
        self.srv.route("DELETE", "/v1/collections/c", json_resp(200, {"id": "u", "slug": "c", "trash_removed": True,
                                                                      "slug_reserved": True, "index_dropped": True}))
        c = self.client.create_collection("new")
        self.assertEqual(c.slug, "new")
        self.assertEqual(self.reqs("POST")[0].json(), {"slug": "new"})
        self.client.create_collection("z", m.Codec.none())
        self.assertEqual(self.reqs("POST")[1].json(), {"slug": "z", "codec": {"algo": "none"}})
        self.assertEqual([x.slug for x in self.client.collections()], ["a", "b"])
        self.assertEqual(self.client.get_collection("c").slug, "c")
        self.assertEqual(self.col.rename("d").slug, "d")
        self.assertEqual(self.reqs("PATCH")[0].json(), {"slug": "d"})
        self.assertTrue(self.col.delete_collection().trash_removed)
        with self.assertRaises(ValueError):
            self.client.collection("")

    def test_status_cached(self):
        self.client.status()
        self.client.status()
        self.assertEqual(len(self.reqs("GET", "/v1/status")), 1)
        self.client.status(refresh=True)
        self.assertEqual(len(self.reqs("GET", "/v1/status")), 2)

    def test_healthz(self):
        self.srv.route("GET", "/healthz", [json_resp(200, {"status": "ok"}),
                                           error_resp(503, "unavailable", "shutting_down")])
        self.assertTrue(self.client.healthz())
        self.assertFalse(self.client.healthz())


class PutTest(Base):
    def setUp(self):
        super().setUp()
        self.srv.route("POST", "/v1/collections/c/records", put_responder)

    def test_put_one_request_known_length(self):
        locs = self.col.put([m.RecordSpec("resource", b"a"), m.RecordSpec("resource", b"b"),
                             m.RecordSpec("resource", io.BytesIO(b"c")), make_warc(2)])
        r = self.reqs("POST")[0]
        self.assertEqual(r.headers["Content-Type"], "application/warc")
        self.assertEqual(int(r.headers["Content-Length"]), len(r.body))
        self.assertEqual(count_records(r.body), 5)
        self.assertEqual(len(locs), 5)

    def test_put_empty_raises_no_request(self):
        with self.assertRaises(ValueError):
            self.col.put([])
        self.assertEqual(self.reqs("POST"), [])

    def test_put_count_mismatch_protocol_error(self):
        self.srv.route("POST", "/v1/collections/c/records", json_resp(201, {"records": []}))
        # later routes do not override earlier ones: use a fresh server route table
        srv2 = FakeServer().start()
        self.addCleanup(srv2.stop)
        srv2.route("POST", "/v1/collections/c/records", json_resp(201, {"records": []}))
        c2 = m.Client(srv2.url, retries=0)
        self.addCleanup(c2.close)
        with self.assertRaises(m.ProtocolError):
            c2.collection("c").put([m.RecordSpec("resource", b"a")])

    def test_put_raw_variants(self):
        self.col.put_raw(make_warc(2))
        self.col.put_raw(io.BytesIO(make_warc(3)))
        self.col.put_raw(iter([make_warc(1)]))
        rs = self.reqs("POST")
        self.assertEqual([count_records(r.body) for r in rs], [2, 3, 1])
        self.assertIn("Content-Length", rs[1].headers)
        self.assertEqual(rs[2].headers["Transfer-Encoding"], "chunked")

    def write_source(self, data, name="s.warc"):
        d = tempfile.TemporaryDirectory()
        self.addCleanup(d.cleanup)
        p = os.path.join(d.name, name)
        with open(p, "wb") as f:
            f.write(data)
        return p

    def test_put_warc_file_batching(self):
        recs = [make_record(i, body=b"x" * 1000) for i in range(25)]
        src = b"".join(recs)
        got = []
        rep = self.col.put_warc_file(self.write_source(src), batch_records=10, on_batch=got.append)
        rs = self.reqs("POST")
        self.assertEqual([count_records(r.body) for r in rs], [10, 10, 5])
        self.assertEqual(b"".join(r.body for r in rs), src)
        self.assertEqual(len(got), 3)
        self.assertEqual((rep.records, rep.batches, rep.bytes, rep.next_record), (25, 3, len(src), 25))

    def test_put_warc_file_batch_bytes(self):
        recs = [make_record(i, body=b"x" * 1000) for i in range(6)]
        big = make_record(99, body=b"y" * 5000)
        src = b"".join(recs[:2] + [big] + recs[2:])
        self.col.put_warc_file(self.write_source(src), batch_bytes=2500)
        sizes = [count_records(r.body) for r in self.reqs("POST")]
        self.assertEqual(sum(sizes), 7)
        self.assertIn(1, sizes)
        self.assertEqual(b"".join(r.body for r in self.reqs("POST")), src)

    def test_put_warc_file_record_too_large(self):
        self.srv._routes[:] = [r for r in self.srv._routes if r[1].pattern != "/v1/status"]
        self.srv.route("GET", "/v1/status", json_resp(200, status_obj(max_put_body_bytes=4096)))
        src = make_record(0, body=b"a" * 100) + make_record(1, body=b"b" * 5000)
        with self.assertRaises(m.PartialPutError) as cm:
            self.col.put_warc_file(self.write_source(src))
        self.assertIsInstance(cm.exception.cause, m.RecordTooLargeError)
        self.assertEqual(cm.exception.report.next_record, 0)
        self.assertFalse(self.reqs("POST")[0].complete)

    def test_put_warc_file_resume_start_record(self):
        src = make_warc(5)
        rep = self.col.put_warc_file(self.write_source(src), start_record=3)
        self.assertEqual((rep.records, rep.next_record), (2, 5))
        self.assertEqual(count_records(self.reqs("POST")[0].body), 2)

    def test_put_warc_file_uncertain_on_drop(self):
        srv = FakeServer().start()
        self.addCleanup(srv.stop)
        srv.route("POST", "/v1/collections/c/records", Drop())
        srv.route("GET", "/v1/status", json_resp(200, status_obj()))
        c = m.Client(srv.url, retries=0)
        self.addCleanup(c.close)
        with self.assertRaises(m.PartialPutError) as cm:
            c.collection("c").put_warc_file(self.write_source(make_warc(3)))
        self.assertEqual(cm.exception.report.uncertain, (0, 3))
        self.assertEqual(cm.exception.report.next_record, 0)

    def test_put_warc_file_gz_source(self):
        src = b"".join(gzip.compress(make_record(i)) for i in range(4))
        rep = self.col.put_warc_file(self.write_source(src, "a.warc.gz"))
        self.assertEqual(rep.records, 4)
        self.assertEqual(self.reqs("POST")[0].body, make_warc(4))


class LookupTest(Base):
    def test_lookup_not_found_returns_none_but_unknown_collection_raises(self):
        self.srv.route("GET", "/v1/collections/c/lookup/by-url", error_resp(404, "not_found", "record_not_found"))
        self.srv.route("GET", "/v1/collections/x/lookup/by-url", error_resp(404, "not_found", "unknown_collection"))
        self.assertIsNone(self.col.lookup_by_url("http://a/"))
        with self.assertRaises(m.NotFoundError):
            self.client.collection("x").lookup_by_url("http://a/")

    def test_sync_only_sent_when_true(self):
        self.srv.route("GET", "/v1/lookup/by-id", json_resp(200, loc_obj(1)))
        self.client.lookup_by_id("urn:uuid:1")
        self.client.lookup_by_id("urn:uuid:1", sync=True, collection="c")
        a, b = self.reqs("GET", "/v1/lookup")
        self.assertNotIn("sync", a.query)
        self.assertEqual(b.query["sync"], ["true"])
        self.assertEqual(b.query["collection"], ["c"])
        self.assertEqual(a.query["fetch"], ["locator"])

    def test_get_by_id_locator_header_and_omitted(self):
        rec = make_record(0)
        h = {"Content-Type": "application/warc"}
        self.srv.route("GET", "/v1/lookup/by-id", [Resp(200, dict(h, **{"Masstuffy-Locator": "L1abc"}), rec),
                                                   Resp(200, dict(h, **{"Masstuffy-Locator-Omitted": "too-long"}), rec)])
        with self.client.get_by_id("x") as r:
            self.assertEqual(r.locator, "L1abc")
            r.read()
        with self.client.get_by_id("x") as r:
            self.assertIsNone(r.locator)

    def test_search_pager_follows_next_until_null(self):
        pages = [{"hits": [loc_obj(1)], "next": "c1"}, {"hits": [loc_obj(2)], "next": "c2"},
                 {"hits": [loc_obj(3)], "next": None}]
        self.srv.route("GET", "/v1/collections/c/search", [json_resp(200, p) for p in pages])
        got = list(self.col.search("a.example", subdomains=True, limit=1, sync=True))
        self.assertEqual([g.offset for g in got], [1, 2, 3])
        qs = [r.query for r in self.reqs("GET", "/v1/collections/c/search")]
        self.assertEqual([q.get("cursor") for q in qs], [None, ["c1"], ["c2"]])
        self.assertEqual(qs[0]["sync"], ["true"])
        self.assertEqual(qs[0]["subdomains"], ["true"])

    def test_scan_pager_stops_on_empty_page(self):
        self.srv.route("GET", "/v1/collections/c/scan", [json_resp(200, {"items": [loc_obj(1)], "next": "n1"}),
                                                         json_resp(200, {"items": [], "next": None})])
        self.assertEqual(len(list(self.col.scan())), 1)

    def test_scan_nonempty_page_null_next_protocol_error(self):
        self.srv.route("GET", "/v1/collections/c/scan", json_resp(200, {"items": [loc_obj(1)], "next": None}))
        with self.assertRaises(m.ProtocolError):
            list(self.col.scan())

    def test_search_param_validation(self):
        for kw in ({"path_match": "exact"}, {"port": "nope"}, {"port": 0}, {"limit": 0}, {"path": "/", "path_match": "x"}):
            with self.assertRaises(ValueError):
                list(self.col.search("h", **kw))
        self.assertEqual(self.reqs("GET", "/v1/collections/c/search"), [])


class ExportTest(Base):
    def pages(self, bodies, unread=None):
        out = []
        for i, b in enumerate(bodies):
            h = {"Content-Type": "application/warc", "Masstuffy-Next-Cursor": "cur%d" % i}
            if b:
                h["Masstuffy-Record-Count"] = str(count_records(b))
            out.append(Resp(200, h, b))
        out.append(Resp(200, {"Content-Type": "application/warc"}, b""))
        return out

    def test_export_pages_to_path(self):
        p1, p2 = make_warc(2), make_warc(3)
        self.srv.route("GET", "/v1/collections/c/export", self.pages([p1, p2]))
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "o.warc")
            rep = self.col.export(path)
            with open(path, "rb") as f:
                self.assertEqual(f.read(), p1 + p2)
            self.assertEqual(os.listdir(d), ["o.warc"])
        self.assertEqual((rep.records, rep.pages, rep.bytes, rep.cursor), (5, 2, len(p1 + p2), "cur1"))
        qs = [r.query.get("cursor") for r in self.reqs("GET", "/v1/collections/c/export")]
        self.assertEqual(qs, [None, ["cur0"], ["cur1"]])

    def test_export_skip_and_all_skipped_page(self):
        p1 = make_warc(1)
        resps = [Resp(200, {"Masstuffy-Next-Cursor": "a", "Masstuffy-Record-Count": "1",
                            "Masstuffy-Unreadable-Count": "2"}, p1),
                 Resp(200, {"Masstuffy-Next-Cursor": "b", "Masstuffy-Record-Count": "0",
                            "Masstuffy-Unreadable-Count": "3"}, b""),
                 Resp(200, {}, b"")]
        self.srv.route("GET", "/v1/collections/c/export", resps)
        out = io.BytesIO()
        rep = self.col.export(out, on_unreadable="skip")
        self.assertEqual((rep.records, rep.unreadable, rep.pages, rep.cursor), (1, 5, 2, "b"))
        self.assertEqual(out.getvalue(), p1)
        self.assertEqual(self.reqs("GET", "/v1/collections/c/export")[0].query["on_unreadable"], ["skip"])

    def test_export_gz_per_record_members(self):
        p1 = make_warc(3)
        self.srv.route("GET", "/v1/collections/c/export", self.pages([p1]))
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "o.warc.gz")
            self.col.export(path)
            with open(path, "rb") as f:
                data = f.read()
        members, out = 0, b""
        while data:
            dobj = zlib.decompressobj(16 + zlib.MAX_WBITS)
            out += dobj.decompress(data)
            data = dobj.unused_data
            members += 1
        self.assertEqual((members, out), (3, p1))

    def test_export_count_mismatch_protocol_error(self):
        self.srv.route("GET", "/v1/collections/c/export", [Resp(200, {"Masstuffy-Next-Cursor": "a",
                                                                       "Masstuffy-Record-Count": "5"}, make_warc(2))])
        with self.assertRaises(m.ExportInterruptedError) as cm:
            self.col.export(io.BytesIO())
        self.assertIsInstance(cm.exception.cause, m.ProtocolError)

    def test_export_interrupted_report(self):
        p1, p2 = make_warc(2), make_warc(2)
        resps = self.pages([p1, p2])
        resps[1] = Resp(200, resps[1].headers, p2[:-30])
        self.srv.route("GET", "/v1/collections/c/export", resps)
        out = io.BytesIO()
        with self.assertRaises(m.ExportInterruptedError) as cm:
            self.col.export(out)
        rep = cm.exception.report
        self.assertEqual((rep.pages, rep.cursor, rep.bytes), (1, "cur0", len(p1)))
        with tempfile.TemporaryDirectory() as d:
            self.srv.requests.clear()
            self.srv._routes[:] = [r for r in self.srv._routes if "export" not in r[1].pattern]
            self.srv.route("GET", "/v1/collections/c/export", resps)
            with self.assertRaises(m.ExportInterruptedError):
                self.col.export(os.path.join(d, "x.warc"))
            self.assertEqual(os.listdir(d), [])

    def test_export_report_bytes_absolute_for_appended_dest(self):
        p1, p2 = make_warc(2), make_warc(2)
        resps = self.pages([p1, p2])
        resps[1] = Resp(200, resps[1].headers, p2[:-30])
        self.srv.route("GET", "/v1/collections/c/export", resps)
        out = io.BytesIO(b"PREFIX-DATA")
        out.seek(0, 2)
        with self.assertRaises(m.ExportInterruptedError) as cm:
            self.col.export(out)
        self.assertEqual(cm.exception.report.bytes, len(b"PREFIX-DATA") + len(p1))
        out.truncate(cm.exception.report.bytes)
        self.assertEqual(out.getvalue(), b"PREFIX-DATA" + p1)

    def test_export_validates_before_opening_destination(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(ValueError):
                self.col.export(os.path.join(d, "o.warc"), on_unreadable="bogus")
            self.assertEqual(os.listdir(d), [])

    def test_export_stale_cursor(self):
        self.srv.route("GET", "/v1/collections/c/export", error_resp(409, "conflict", "stale_locator"))
        with self.assertRaises(m.ExportInterruptedError) as cm:
            self.col.export(io.BytesIO(), cursor="old")
        self.assertIsInstance(cm.exception.cause, m.StaleLocatorError)

    def test_iter_export_records(self):
        self.srv.route("GET", "/v1/collections/c/export", self.pages([make_warc(2), make_warc(1)]))
        ids = [r.record_id for r in self.col.iter_export()]
        self.assertEqual(len(ids), 3)


class DeleteJobTest(Base):
    def test_delete_variants(self):
        self.srv.route("POST", "/v1/collections/c/records/delete", json_resp(200, {"deleted": 2}))
        self.srv.route("DELETE", "/v1/collections/c/records/L1a", json_resp(200, {"deleted": 1}))
        self.srv.route("POST", "/v1/collections/c/records/delete-by-id",
                       [json_resp(200, {"deleted": loc_obj(1)}), error_resp(404, "not_found", "record_not_found")])
        self.assertEqual(self.col.delete(["L1a", "L1b"]), 2)
        self.assertEqual(self.reqs("POST")[0].json(), {"locators": ["L1a", "L1b"]})
        self.assertEqual(self.col.delete_one("L1a"), 1)
        self.assertEqual(self.col.delete_by_id("x").offset, 1)
        self.assertIsNone(self.col.delete_by_id("x"))
        with self.assertRaises(ValueError):
            self.col.delete(["L1a"] * 65537)

    def job(self, state, **kw):
        o = {"id": "7", "kind": "train", "collection": "c", "collection_id": "u", "state": state,
             "started_at": "t", "finished_at": None, "result": None, "error": None}
        o.update(kw)
        return json_resp(200, o)

    def test_job_wait_long_poll(self):
        self.srv.route("POST", "/v1/collections/c/train", Resp(202, {"Content-Type": "application/json"},
                                                               self.job("running").body))
        self.srv.route("GET", "/v1/jobs/7", [self.job("running"), self.job("running"),
                                            self.job("succeeded", result={"dict_id": 5})])
        j = self.col.train()
        self.assertEqual(self.reqs("POST")[0].json(), {})
        j.wait(timeout=100)
        self.assertEqual(j.result, {"dict_id": 5})
        waits = [int(r.query["wait"][0]) for r in self.reqs("GET", "/v1/jobs/7")]
        self.assertEqual(len(waits), 3)
        self.assertTrue(all(1 <= w <= 60 for w in waits))

    def test_job_failed_and_timeout(self):
        err = {"kind": "corrupt", "code": "train_failed", "message": "no"}
        self.srv.route("GET", "/v1/jobs/7", self.job("failed", error=err))
        with self.assertRaises(m.JobFailedError) as cm:
            self.client.job("7").wait()
        self.assertEqual(cm.exception.error.code, "train_failed")
        self.client.job("7").wait(raise_on_failure=False)
        srv = FakeServer().start()
        self.addCleanup(srv.stop)
        srv.route("GET", "/v1/jobs/7", self.job("running"))
        c = m.Client(srv.url)
        self.addCleanup(c.close)
        with self.assertRaises(m.JobTimeoutError):
            c.job("7").wait(timeout=0.0)

    def test_rebuild_and_train_request_bodies(self):
        ok = Resp(202, {"Content-Type": "application/json"}, self.job("running").body)
        self.srv.route("POST", "/v1/collections/c/rebuild", ok)
        self.srv.route("POST", "/v1/collections/c/train", ok)
        self.col.rebuild(m.Codec.zstd(9, 4))
        self.col.rebuild(m.Codec.none(), recompress=True, workers=2)
        self.col.train(max_dict_size=100)
        b = [r.json() for r in self.reqs("POST")]
        self.assertEqual(b[0], {"codec": {"algo": "zstd", "dict_id": 9, "level": 4}, "recompress": False,
                                "on_unreadable": "fail"})
        self.assertEqual(b[1]["workers"], 2)
        self.assertNotIn("max_inflight_bytes", b[1])
        self.assertEqual(b[2], {"max_dict_size": 100})


REPORT = {"format": "v1", "records": 1, "batches": 1, "rejected": 0, "rejections": [], "committed_offset": 9}


class ImportEnvelopeTest(Base):
    def test_import_envelope_sources(self):
        self.srv.route("POST", "/v1/collections/c/import", json_resp(200, REPORT))
        data = b"#masstuffy-cdx/1 gen=0\nline1\npayload"
        self.col.import_envelope(data)
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "e")
            with open(p, "wb") as f:
                f.write(data)
            self.col.import_envelope(p, on_invalid="skip", batch_records=5)
            self.col.import_envelope(p, resume_offset=30)
        self.col.import_envelope(iter([b"a", b"b"]))
        rs = self.reqs("POST")
        self.assertEqual(rs[0].body, data)
        self.assertEqual(int(rs[1].headers["Content-Length"]), len(data))
        self.assertEqual(rs[1].query["on_invalid"], ["skip"])
        self.assertEqual(rs[1].query["batch_records"], ["5"])
        self.assertEqual(rs[2].body, b"#masstuffy-cdx/1 gen=0\n" + data[30:])
        self.assertEqual(rs[3].headers["Transfer-Encoding"], "chunked")
        self.assertEqual(rs[0].headers["Content-Type"], "application/x-masstuffy-envelope")
        with self.assertRaises(ValueError):
            self.col.import_envelope(b"x", on_invalid="nope")
        with self.assertRaises(ValueError):
            self.col.import_envelope(iter([b"a"]), resume_offset=3)

    def test_import_error_exposes_report(self):
        self.srv.route("POST", "/v1/collections/c/import",
                       error_resp(400, "invalid", "import_failed", details={"report": REPORT}))
        with self.assertRaises(m.InvalidRequestError) as cm:
            self.col.import_envelope(b"x")
        self.assertEqual(cm.exception.import_report.records, 1)


@unittest.skipUnless(have_xxhash(), "xxhash not installed")
class NoneImportTest(Base):
    def test_import_records_none_collection_no_zstd_needed(self):
        self.srv.route("POST", "/v1/collections/c/import", json_resp(200, REPORT))
        rep = self.col.import_records([m.RecordSpec("resource", b"abc"), make_record(1)])
        self.assertEqual(rep.records, 1)
        body = self.reqs("POST")[0].body
        self.assertTrue(body.startswith(b"#masstuffy-cdx/1 gen=0\nA 0 0 "))
        self.assertIn(b"urn:uuid:00000000-0000-0000-0000-000000000001", body)
        self.assertEqual([f[6] for f, _ in parse_v1(body)], ["resource", "resource"])
        self.assertEqual(parse_v1(body)[1][1], make_record(1))
        with self.assertRaises(ValueError):
            self.col.import_records([b"not a record"])
        with self.assertRaises(ValueError):
            self.col.import_records([make_warc(2)])

    def test_put_warc_file_mode_import_none(self):
        self.srv.route("POST", "/v1/collections/c/import", json_resp(200, dict(REPORT, records=3, batches=1)))
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "s.warc")
            with open(p, "wb") as f:
                f.write(make_warc(3))
            rep = self.col.put_warc_file(p, mode="import")
            self.assertEqual((rep.records, rep.next_record), (3, 3))
            self.assertEqual(rep.bytes, len(make_warc(3)))
            with self.assertRaises(ValueError):
                self.col.put_warc_file(p, mode="import", on_batch=print)
        self.assertEqual(self.reqs("POST")[0].headers["Transfer-Encoding"], "chunked")


class RawTest(Base):
    def raw_resp(self, body, **h):
        import xxhash
        hd = {"Masstuffy-Encoding": "warc", "Masstuffy-Raw-Size": str(len(body)),
              "Masstuffy-Digest": "%016x" % xxhash.xxh3_64_intdigest(body), "Content-Type": "application/warc"}
        hd.update(h)
        hd = {k: v for k, v in hd.items() if v is not None}
        return Resp(200, hd, body)

    @unittest.skipUnless(have_xxhash(), "xxhash not installed")
    def test_get_raw_headers_and_digest(self):
        rec = make_record(0)
        self.srv.route("GET", "/v1/collections/c/records/L1a/raw", [
            self.raw_resp(rec), self.raw_resp(rec, **{"Masstuffy-Digest": "f" * 16}),
            self.raw_resp(rec, **{"Masstuffy-Encoding": None})])
        with self.col.get_raw("L1a") as rb:
            self.assertEqual((rb.encoding, rb.raw_size), ("warc", len(rec)))
            self.assertEqual(rb.read(), rec)
        with self.assertRaises(m.ProtocolError):
            with self.col.get_raw("L1a") as rb:
                rb.read()
        with self.assertRaises(m.ProtocolError):
            self.col.get_raw("L1a")

    @unittest.skipUnless(have_xxhash(), "xxhash not installed")
    def test_get_decode_client_none(self):
        rec = make_record(0)
        self.srv.route("GET", "/v1/collections/c/records/L1a/raw", self.raw_resp(rec))
        with self.col.get("L1a", decode="client") as r:
            self.assertEqual(r.read(), b"body 0 " * 20)

    @unittest.skipUnless(have_zstd() and have_xxhash(), "zstandard/xxhash not installed")
    def test_zstd_decompress_and_client_compression(self):
        import zstandard
        import xxhash
        from tests.helpers import train_local_dict
        did, dbytes = train_local_dict()
        self.srv.route("GET", "/v1/dictionaries/%d" % did, Resp(200, {"Content-Type": "application/octet-stream"}, dbytes))
        cd = zstandard.ZstdCompressionDict(dbytes)
        rec = make_record(0, body=b"<html>text</html>" * 20)
        frame = zstandard.ZstdCompressor(dict_data=cd, write_checksum=False, write_content_size=True,
                                         write_dict_id=True).compress(rec)
        hd = {"Masstuffy-Encoding": "zstd", "Masstuffy-Dict-Id": str(did), "Masstuffy-Raw-Size": str(len(frame)),
              "Masstuffy-Digest": "%016x" % xxhash.xxh3_64_intdigest(frame),
              "Masstuffy-Content-Size": str(len(rec)), "Content-Type": "application/zstd"}
        self.srv.route("GET", "/v1/collections/c/records/L1a/raw", Resp(200, hd, frame))
        for _ in range(3):
            with self.col.get_raw("L1a", decompress=True) as rb:
                self.assertEqual((rb.encoding, rb.dict_id), ("warc", did))
                self.assertEqual(rb.read(), rec)
        self.assertEqual(len(self.reqs("GET", "/v1/dictionaries/")), 1)
        with self.col.get("L1a", decode="client") as r:
            self.assertEqual(r.record_id, "urn:uuid:00000000-0000-0000-0000-000000000000")
            r.read()
        with self.col.get_raw("L1a") as rb:
            self.assertEqual(rb.encoding, "zstd")
            self.assertEqual(rb.read(), frame)

    @unittest.skipUnless(have_zstd() and have_xxhash(), "zstandard/xxhash not installed")
    def test_import_records_client_compress_and_codec_retry(self):
        import zstandard
        from tests.helpers import train_local_dict
        did, dbytes = train_local_dict()
        did2, dbytes2 = train_local_dict(8192, 250)
        if did2 == did:
            self.skipTest("same dictionary")
        zrules = lambda i: dict(RULES_NONE, payload="zstd_frame", dict_id=i)
        zcodec = lambda i: {"algo": "zstd", "dict_id": i, "level": 3}
        self.srv._routes[:] = [r for r in self.srv._routes if r[1].pattern != "/v1/collections/c"]
        self.srv.route("GET", "/v1/collections/c", [json_resp(200, col_obj(codec=zcodec(did), rules=zrules(did))),
                                                    json_resp(200, col_obj(codec=zcodec(did2), rules=zrules(did2)))])
        self.srv.route("GET", "/v1/dictionaries/%d" % did, Resp(200, {}, dbytes))
        self.srv.route("GET", "/v1/dictionaries/%d" % did2, Resp(200, {}, dbytes2))
        self.srv.route("POST", "/v1/collections/c/import", [
            error_resp(400, "invalid", "import_failed", details={"report": dict(REPORT, records=1)}),
            json_resp(200, dict(REPORT, records=2))])
        specs = [m.RecordSpec("resource", b"a" * 300), m.RecordSpec("resource", b"b" * 300),
                 m.RecordSpec("resource", b"c" * 300)]
        rep = self.col.import_records(specs)
        self.assertEqual(rep.records, 3)  # 1 committed before the failure + 2 retried
        posts = self.reqs("POST")
        self.assertEqual(len(posts), 2)
        self.assertEqual(len(parse_v1(posts[0].body)), 3)
        self.assertEqual(len(parse_v1(posts[1].body)), 2)
        self.assertEqual(len(self.reqs("GET", "/v1/dictionaries/")), 2)
        # frames of the retry use dict 2
        for _, payload in parse_v1(posts[1].body):
            self.assertEqual(zstandard.get_frame_parameters(payload).dict_id, did2)
        for _, payload in parse_v1(posts[0].body):
            self.assertEqual(zstandard.get_frame_parameters(payload).dict_id, did)


@unittest.skipUnless(have_xxhash(), "xxhash not installed")
class ImportRetryFailureTest(Base):
    def test_unchanged_codec_second_failure_not_retried(self):
        self.srv.route("POST", "/v1/collections/c/import",
                       error_resp(400, "invalid", "import_failed", details={"report": dict(REPORT, records=1)}))
        with self.assertRaises(m.InvalidRequestError) as cm:
            self.col.import_records([m.RecordSpec("resource", b"a"), m.RecordSpec("resource", b"b")])
        self.assertEqual(cm.exception.import_report.records, 1)
        self.assertEqual(len(self.reqs("POST")), 1)
        self.assertEqual(len(self.reqs("GET", "/v1/collections/c")), 2)  # info, then the refresh that found no change

    def _codec_change_server(self):
        from tests.helpers import train_local_dict
        did, dbytes = train_local_dict()
        did2, dbytes2 = train_local_dict(8192, 250)
        if did2 == did:
            self.skipTest("same dictionary")
        rules = lambda i: dict(RULES_NONE, payload="zstd_frame", dict_id=i)
        codec = lambda i: {"algo": "zstd", "dict_id": i, "level": 3}
        self.srv._routes[:] = [r for r in self.srv._routes if r[1].pattern != "/v1/collections/c"]
        self.srv.route("GET", "/v1/collections/c", [json_resp(200, col_obj(codec=codec(did), rules=rules(did))),
                                                    json_resp(200, col_obj(codec=codec(did2), rules=rules(did2)))])
        self.srv.route("GET", "/v1/dictionaries/%d" % did, Resp(200, {}, dbytes))
        self.srv.route("GET", "/v1/dictionaries/%d" % did2, Resp(200, {}, dbytes2))
        err = lambda n: error_resp(400, "invalid", "import_failed", details={"report": dict(REPORT, records=n)})
        self.srv.route("POST", "/v1/collections/c/import", [err(4), err(1)])

    def test_retry_failure_keeps_committed_prefix_import_records(self):
        if not have_zstd():
            self.skipTest("zstandard not installed")
        self._codec_change_server()
        specs = [m.RecordSpec("resource", b"x%d" % i * 50) for i in range(8)]
        with self.assertRaises(m.PartialPutError) as cm:
            self.col.import_records(specs)
        self.assertEqual(cm.exception.report.records, 5)  # 4 before the retry + 1 in it
        self.assertEqual(len(self.reqs("POST")), 2)

    def test_retry_failure_keeps_committed_prefix_put_warc_file(self):
        if not have_zstd():
            self.skipTest("zstandard not installed")
        self._codec_change_server()
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "s.warc")
            with open(path, "wb") as f:
                f.write(make_warc(8))
            with self.assertRaises(m.PartialPutError) as cm:
                self.col.put_warc_file(path, mode="import", start_record=2)
        rep = cm.exception.report
        self.assertEqual((rep.records, rep.next_record), (5, 7))
        self.assertIsNone(rep.uncertain)

    def test_non_api_failure_marks_uncertain(self):
        self.srv.route("POST", "/v1/collections/c/import", Drop())
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "s.warc")
            with open(path, "wb") as f:
                f.write(make_warc(3))
            with self.assertRaises(m.PartialPutError) as cm:
                self.col.put_warc_file(path, mode="import")
        self.assertEqual(cm.exception.report.uncertain, (0, -1))


class ErrorMappingTest(Base):
    def test_malformed_delete_bodies_are_protocol_errors(self):
        self.srv.route("POST", "/v1/collections/c/records/delete", json_resp(200, {"nope": 1}))
        self.srv.route("DELETE", "/v1/collections/c/records/L1a", json_resp(200, ["x"]))
        with self.assertRaises(m.ProtocolError):
            self.col.delete(["L1a"])
        with self.assertRaises(m.ProtocolError):
            self.col.delete_one("L1a")

    def test_put_batch_counted_before_locator_check(self):
        self.srv.route("POST", "/v1/collections/c/records", json_resp(201, {"records": []}))
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "s.warc")
            with open(path, "wb") as f:
                f.write(make_warc(2))
            with self.assertRaises(m.PartialPutError) as cm:
                self.col.put_warc_file(path)
        self.assertEqual((cm.exception.report.records, cm.exception.report.next_record), (2, 2))

    def test_error_classes_through_client(self):
        self.srv.route("GET", "/v1/collections/x", error_resp(404, "not_found", "unknown_collection"))
        with self.assertRaises(m.NotFoundError):
            self.client.get_collection("x")
        self.srv.route("GET", "/v1/jobs/9", error_resp(404, "not_found", "job_not_found"))
        with self.assertRaises(m.NotFoundError):
            self.client.job("9")


if __name__ == "__main__":
    unittest.main()
