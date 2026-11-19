import unittest

from masstuffy.errors import ProtocolError
from masstuffy.models import (GrepCollectionReport, GrepHit, GrepProgress, GrepQuery, GrepReport, GrepUnreadable,
                              Codec, CollectionInfo, CollectionSize, DeletedCollection, Locator, ServerStatus, RawRules,
                              locator_token)

LOC = {"locator": "L1abc", "collection": "u", "generation": 1, "segment": 0, "offset": 5, "raw_size": 9,
       "digest": "0" * 16, "date": "2020-01-01T00:00:00.123456789Z", "warc_type": "response",
       "record_id": "urn:uuid:x", "url": "http://a/"}
RULES = {"payload": "warc", "single_frame": True, "dict_id": None, "dictless_min_content_size": 1048577}
LIMITS = {"max_put_body_bytes": 1, "max_put_records": 2, "max_import_body_bytes": None, "max_json_body_bytes": 3,
          "max_export_page_bytes": 4, "request_timeout_ms": 5, "body_idle_timeout_ms": 6,
          "max_concurrent_requests": 7}


class ModelsTest(unittest.TestCase):
    def test_locator_from_json_roundtrip_fields(self):
        loc = Locator.from_json(LOC)
        self.assertEqual((loc.token, loc.collection, loc.generation, loc.offset, loc.url),
                         ("L1abc", "u", 1, 5, "http://a/"))
        self.assertEqual(str(loc), "L1abc")

    def test_locator_url_null(self):
        self.assertIsNone(Locator.from_json(dict(LOC, url=None)).url)

    def test_locator_datetime_nanos_truncated(self):
        self.assertEqual(Locator.from_json(LOC).datetime.microsecond, 123456)

    def test_missing_field_protocol_error(self):
        d = dict(LOC)
        del d["offset"]
        with self.assertRaises(ProtocolError):
            Locator.from_json(d)

    def test_bool_not_int(self):
        with self.assertRaises(ProtocolError):
            Locator.from_json(dict(LOC, offset=True))

    def test_unknown_keys_ignored(self):
        Locator.from_json(dict(LOC, future="x"))

    def test_codec_to_json(self):
        self.assertEqual(Codec.none().to_json(), {"algo": "none"})
        self.assertEqual(Codec.zstd(7).to_json(), {"algo": "zstd", "dict_id": 7, "level": 3})

    def test_codec_zstd_bad_dict_id(self):
        for bad in (0, -1, 2**32):
            with self.assertRaises(ValueError):
                Codec.zstd(bad)

    def test_collection_info_with_and_without_stats(self):
        base = {"id": "u", "slug": "s", "generation": 0, "codec": {"algo": "none"}, "raw_rules": RULES}
        self.assertIsNone(CollectionInfo.from_json(base).stats)
        ci = CollectionInfo.from_json(dict(base, stats={"commits": 1, "records": 2, "cdx_len": 3}))
        self.assertEqual(ci.stats.records, 2)

    def test_collection_info_size(self):
        base = {"id": "u", "slug": "s", "generation": 1, "codec": {"algo": "none"}, "raw_rules": RULES}
        self.assertIsNone(CollectionInfo.from_json(base).size)
        size = {"records": 1, "records_total": 2, "tombstones": 1, "stored_bytes": 30, "tombstoned_bytes": 10,
                "segment_bytes": 40, "cdx_bytes": 50}
        ci = CollectionInfo.from_json(dict(base, size=size))
        self.assertEqual(ci.size, CollectionSize(1, 2, 1, 30, 10, 40, 50))
        with self.assertRaises(ProtocolError):
            CollectionInfo.from_json(dict(base, size=dict(size, records=True)))
        missing = dict(size)
        del missing["cdx_bytes"]
        with self.assertRaises(ProtocolError):
            CollectionInfo.from_json(dict(base, size=missing))

    def test_raw_rules(self):
        r = RawRules.from_json(RULES)
        self.assertEqual((r.payload, r.single_frame, r.dict_id), ("warc", True, None))

    def test_status_limits_import_null(self):
        st = ServerStatus.from_json({"version": "v", "api": 1, "repository": "r", "raw_check": "full",
                                     "max_record_bytes": 9, "collections": 0, "dictionaries": 0,
                                     "jobs_running": 0, "limits": LIMITS, "open_report": None})
        self.assertIsNone(st.limits.max_import_body_bytes)
        self.assertIsNone(st.open_report)

    def test_deleted_lenient(self):
        d = DeletedCollection.from_json({"id": "u", "slug": "s", "trash_removed": True})
        self.assertIsNone(d.index_dropped)

    def test_locator_token_validation(self):
        for bad in ("", "X1abc", "L1é", "L1" + "a" * 360_447):
            with self.assertRaises(ValueError):
                locator_token(bad)
        with self.assertRaises(TypeError):
            locator_token(5)
        self.assertEqual(locator_token("L1ok"), "L1ok")
        self.assertEqual(locator_token(Locator.from_json(LOC)), "L1abc")

    def test_grep_models(self):
        self.assertEqual(GrepQuery("a", regex=True).to_json(),
                         {"pattern": "a", "regex": True, "case_insensitive": False})
        h = GrepHit.from_json({"slug": "s", "queries": [0, 3], "record": LOC})
        self.assertEqual((h.slug, h.queries, h.locator.token), ("s", [0, 3], "L1abc"))
        with self.assertRaises(ProtocolError):
            GrepHit.from_json({"slug": "s", "queries": ["x"], "record": LOC})
        u = GrepUnreadable.from_json({"slug": "s", "record": LOC,
                                      "error": {"kind": "corrupt", "code": "unreadable_record", "message": "m"}})
        self.assertEqual(u.error.code, "unreadable_record")
        p = GrepProgress.from_json({"slug": "s", "records": 1, "bytes": 2, "cdx_done": 1, "cdx_total": 4})
        self.assertEqual(p.fraction, 0.25)
        self.assertEqual(GrepProgress("s", 0, 0, 0, 0).fraction, 1.0)

    def test_grep_report(self):
        col = {"slug": "s", "id": "u", "status": "complete", "records": 1, "bytes": 2, "cdx_done": 4,
               "cdx_total": 4, "hits": 1, "unreadable": 0, "error": None}
        rep = GrepReport.from_json({"hits": 1, "truncated": False, "collections": [col]})
        self.assertTrue(rep.complete)
        self.assertIsNone(rep.collections[0].error)
        with self.assertRaises(ProtocolError):
            GrepCollectionReport.from_json(dict(col, status="weird"))
        with self.assertRaises(ProtocolError):
            GrepCollectionReport.from_json({k: v for k, v in col.items() if k != "cdx_total"})
        bad = GrepCollectionReport.from_json(dict(col, status="failed",
                                                  error={"kind": "conflict", "code": "stale_locator",
                                                         "message": "m"}))
        self.assertEqual(bad.error.code, "stale_locator")


if __name__ == "__main__":
    unittest.main()
