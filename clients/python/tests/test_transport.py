import threading
import time
import unittest
from unittest import mock

from masstuffy._transport import Transport, seg
from masstuffy.errors import NotFoundError, PayloadTooLargeError, TransportError, UnavailableError
from tests.fakeserver import Drop, FakeServer, Hang, Resp, error_resp, json_resp


_OPEN = []


def mk(server, **kw):
    kw.setdefault("retries", 2)
    kw.setdefault("backoff", 0.01)
    t = Transport(server.url, **kw)
    _OPEN.append(t)
    return t


class TransportTest(unittest.TestCase):
    def setUp(self):
        self.srv = FakeServer().start()
        self.addCleanup(self.srv.stop)
        self.addCleanup(lambda: [t.close() for t in _OPEN])

    def test_keepalive_reuses_connection(self):
        self.srv.route("GET", "/x", json_resp(200, {"a": 1}))
        t = mk(self.srv)
        for _ in range(3):
            self.assertEqual(t.request("GET", "/x").json(), {"a": 1})
        self.assertEqual(self.srv.connections, 1)

    def test_stale_idle_connection_discarded(self):
        self.srv.route("GET", "/x", Resp(200, {"Content-Type": "application/json"}, b"{}", close=True))
        t = mk(self.srv, retries=0)
        t.request("GET", "/x")
        time.sleep(0.1)
        t.request("GET", "/x")
        self.assertEqual(self.srv.connections, 2)

    def test_idle_age_discard(self):
        self.srv.route("GET", "/x", json_resp(200, {}))
        t = mk(self.srv)
        t.request("GET", "/x")
        real = time.monotonic
        with mock.patch("masstuffy._transport.time.monotonic", lambda: real() + 16):
            t.request("GET", "/x")
        self.assertEqual(self.srv.connections, 2)

    def test_retry_overloaded_then_ok(self):
        self.srv.route("GET", "/x", [error_resp(503, "unavailable", "overloaded", retry_after=0), json_resp(200, {})])
        mk(self.srv).request("GET", "/x")
        self.assertEqual(len(self.srv.requests), 2)

    def test_retry_overloaded_post_bytes_body(self):
        self.srv.route("POST", "/x", [error_resp(503, "unavailable", "overloaded", retry_after=0), json_resp(200, {})])
        mk(self.srv).request("POST", "/x", json_body={"a": 1})
        self.assertEqual(len(self.srv.requests), 2)
        self.assertEqual(self.srv.requests[1].json(), {"a": 1})

    def test_no_retry_timeout_code(self):
        self.srv.route("GET", "/x", error_resp(503, "unavailable", "timeout"))
        with self.assertRaises(UnavailableError):
            mk(self.srv).request("GET", "/x")
        self.assertEqual(len(self.srv.requests), 1)

    def test_no_retry_streaming_body(self):
        self.srv.route("POST", "/x", error_resp(503, "unavailable", "overloaded", retry_after=0))
        with self.assertRaises(UnavailableError):
            mk(self.srv).request("POST", "/x", body=iter([b"abc"]))
        self.assertEqual(len(self.srv.requests), 1)

    def test_callable_body_is_replayed(self):
        self.srv.route("POST", "/x", [error_resp(503, "unavailable", "overloaded", retry_after=0), json_resp(200, {})])
        mk(self.srv).request("POST", "/x", body=lambda: iter([b"ab", b"cd"]), content_length=4)
        self.assertEqual([r.body for r in self.srv.requests], [b"abcd", b"abcd"])

    def test_retry_transport_error_get(self):
        self.srv.route("GET", "/x", [Drop(), json_resp(200, {})])
        mk(self.srv).request("GET", "/x")
        self.assertEqual(len(self.srv.requests), 2)

    def test_post_drop_outcome_unknown(self):
        self.srv.route("POST", "/x", Drop())
        with self.assertRaises(TransportError) as cm:
            mk(self.srv).request("POST", "/x", json_body={})
        self.assertTrue(cm.exception.outcome_unknown)
        self.assertEqual(len(self.srv.requests), 1)

    def test_idempotent_post_retried(self):
        self.srv.route("POST", "/x", [Drop(), Resp(204)])
        mk(self.srv).request("POST", "/x", idempotent=True, ok=(204,))
        self.assertEqual(len(self.srv.requests), 2)

    def test_retries_exhausted(self):
        self.srv.route("GET", "/x", error_resp(503, "unavailable", "overloaded", retry_after=0))
        with self.assertRaises(UnavailableError):
            mk(self.srv, retries=2).request("GET", "/x")
        self.assertEqual(len(self.srv.requests), 3)

    def test_backoff_capped_by_max_backoff(self):
        self.srv.route("GET", "/x", error_resp(503, "unavailable", "overloaded", retry_after=100))
        sleeps = []
        with mock.patch("masstuffy._transport.time.sleep", sleeps.append):
            with self.assertRaises(UnavailableError):
                mk(self.srv, max_backoff=3.0).request("GET", "/x")
        self.assertEqual(sleeps, [3.0, 3.0])

    def test_chunked_body_sent(self):
        self.srv.route("POST", "/x", json_resp(200, {}))
        mk(self.srv).request("POST", "/x", body=iter([b"abc", b"def"]))
        r = self.srv.requests[0]
        self.assertEqual(r.headers["Transfer-Encoding"], "chunked")
        self.assertEqual(r.body, b"abcdef")

    def test_known_length_iterable(self):
        self.srv.route("POST", "/x", json_resp(200, {}))
        mk(self.srv).request("POST", "/x", body=iter([b"abc", b"def"]), content_length=6)
        r = self.srv.requests[0]
        self.assertEqual(r.headers["Content-Length"], "6")
        self.assertIsNone(r.headers.get("Transfer-Encoding"))
        self.assertEqual(r.body, b"abcdef")

    def test_body_iterator_exception_propagates_and_conn_not_pooled(self):
        self.srv.route("POST", "/x", json_resp(200, {}))
        self.srv.route("GET", "/y", json_resp(200, {}))

        def gen():
            yield b"abc"
            raise ValueError("boom")

        t = mk(self.srv)
        with self.assertRaises(ValueError) as cm:
            t.request("POST", "/x", body=gen())
        self.assertEqual(str(cm.exception), "boom")
        t.request("GET", "/y")
        self.assertEqual(self.srv.connections, 2)
        self.assertFalse(self.srv.requests[0].complete)

    def test_body_iterator_oserror_propagates_unchanged(self):
        self.srv.route("POST", "/x", json_resp(200, {}))

        def gen():
            yield b"abc"
            raise OSError("disk")

        with self.assertRaises(OSError) as cm:
            mk(self.srv).request("POST", "/x", body=gen())
        self.assertNotIsInstance(cm.exception, TransportError)

    def test_early_error_response_during_upload(self):
        self.srv.route("POST", "/x", Resp(413, {"Content-Type": "application/json"},
                                           error_resp(413, "invalid", "payload_too_large").body, read_body=False))

        def gen():
            for _ in range(160):
                yield b"x" * 65536

        with self.assertRaises(PayloadTooLargeError):
            mk(self.srv).request("POST", "/x", body=gen())

    def test_streaming_response_partial_close_closes_conn(self):
        self.srv.route("GET", "/big", Resp(200, {}, b"z" * (1 << 20)))
        t = mk(self.srv)
        r = t.request("GET", "/big", stream=True)
        self.assertEqual(len(r.read(10)), 10)
        r.close()
        r = t.request("GET", "/big", stream=True)
        self.assertEqual(self.srv.connections, 2)
        self.assertEqual(len(r.read()), 1 << 20)
        r.close()
        t.request("GET", "/big", stream=True).close()
        self.assertEqual(self.srv.connections, 2)

    def test_socket_timeout(self):
        self.srv.route("GET", "/slow", Hang(2))
        with self.assertRaises(TransportError):
            mk(self.srv, retries=0).request("GET", "/slow", timeout=0.3)

    def test_error_response_not_found(self):
        self.srv.route("GET", "/x", error_resp(404, "not_found", "unknown_collection"))
        with self.assertRaises(NotFoundError):
            mk(self.srv).request("GET", "/x")

    def test_query_encoding(self):
        self.srv.route("GET", "/c/a b/x", json_resp(200, {}))
        mk(self.srv).request("GET", "/c/%s/x" % seg("a b"), query=[("q", "a b+c&d=é")])
        r = self.srv.requests[0]
        self.assertEqual(r.q("q"), "a b+c&d=é")
        self.assertEqual(r.path, "/c/a b/x")
        self.assertIn("a%20b%2Bc%26d%3D%C3%A9", r.raw_path)

    def test_base_url_prefix_and_validation(self):
        self.srv.route("GET", "/prefix/x", json_resp(200, {}))
        t = Transport(self.srv.url + "/prefix/")
        _OPEN.append(t)
        t.request("GET", "/x")
        with self.assertRaises(ValueError):
            Transport("ftp://h/")
        with self.assertRaises(ValueError):
            Transport("http://h/?q=1")

    def test_thread_safety_pool(self):
        self.srv.route("GET", "/x", json_resp(200, {}))
        t = mk(self.srv, pool_size=4)
        errs = []

        def work():
            try:
                for _ in range(20):
                    t.request("GET", "/x")
            except Exception as e:  # pragma: no cover
                errs.append(e)

        ths = [threading.Thread(target=work) for _ in range(8)]
        [x.start() for x in ths]
        [x.join() for x in ths]
        self.assertEqual(errs, [])
        self.assertLessEqual(self.srv.connections, 12)
        self.assertLessEqual(t.idle_count(), 4)


if __name__ == "__main__":
    unittest.main()
