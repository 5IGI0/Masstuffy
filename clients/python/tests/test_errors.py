import json
import unittest

from masstuffy import errors as E


def body(kind, code, message="m", details=None):
    err = {"kind": kind, "code": code, "message": message}
    if details is not None:
        err["details"] = details
    return json.dumps({"error": err}).encode()


class ErrorsTest(unittest.TestCase):
    def test_kind_mapping(self):
        cases = [
            (400, "invalid", "invalid_param", E.InvalidRequestError),
            (413, "invalid", "payload_too_large", E.PayloadTooLargeError),
            (404, "not_found", "unknown_collection", E.NotFoundError),
            (409, "conflict", "slug_taken", E.ConflictError),
            (409, "conflict", "stale_locator", E.StaleLocatorError),
            (422, "corrupt", "not_decodable", E.NotDecodableError),
            (503, "unavailable", "overloaded", E.UnavailableError),
            (500, "io", "io", E.ServerError),
            (500, "internal", "internal", E.ServerError),
            (500, "weird_new_kind", "x", E.ServerError),
            (418, "weird", "x", E.InvalidRequestError),
        ]
        for status, kind, code, cls in cases:
            with self.subTest(code=code):
                e = E.error_from_response(status, body(kind, code, "msg", {"a": 1}), {}, "GET", "/p")
                self.assertIs(type(e), cls)
                self.assertEqual((e.status, e.kind, e.code, e.message, e.details), (status, kind, code, "msg", {"a": 1}))
                self.assertEqual((e.method, e.path), ("GET", "/p"))
                self.assertEqual(str(e), f"{status} {code}: msg")

    def test_non_json_body(self):
        e = E.error_from_response(502, b"<html>" + b"x" * 500, {}, "GET", "/")
        self.assertIsInstance(e, E.ServerError)
        self.assertEqual(e.code, "http_502")
        self.assertEqual(len(e.message), 200)
        e = E.error_from_response(400, b"nope", {}, "GET", "/")
        self.assertIsInstance(e, E.InvalidRequestError)

    def test_retry_after_parsed(self):
        e = E.error_from_response(503, body("unavailable", "overloaded"), {"Retry-After": "5"}, "GET", "/")
        self.assertEqual(e.retry_after, 5.0)

    def test_retry_after_invalid_ignored(self):
        for v in ("-1", "abc", "1.5", ""):
            e = E.error_from_response(503, body("unavailable", "overloaded"), {"Retry-After": v}, "GET", "/")
            self.assertIsNone(e.retry_after)

    def test_import_report_property(self):
        rep = {"format": "v1", "records": 2, "batches": 1, "rejected": 0, "rejections": [], "committed_offset": 10}
        e = E.error_from_response(400, body("invalid", "import_failed", details={"report": rep}), {}, "POST", "/")
        self.assertEqual(e.import_report.records, 2)
        e = E.error_from_response(400, body("invalid", "x"), {}, "POST", "/")
        self.assertIsNone(e.import_report)

    def test_error_from_object_job(self):
        e = E.error_from_object({"kind": "corrupt", "code": "read_failed", "message": "bad"})
        self.assertIsInstance(e, E.ServerError)
        self.assertIsNone(e.status)
        self.assertEqual(str(e), "read_failed: bad")
        self.assertIsInstance(E.error_from_object({"kind": "unavailable", "code": "shutting_down"}), E.UnavailableError)


if __name__ == "__main__":
    unittest.main()
