import io
import unittest

import pywarc

import masstuffy as m
from masstuffy._transport import Transport
from masstuffy.records import Record, _prepare_spec, _put_body
from tests.fakeserver import FakeServer, Resp
from tests.helpers import make_record, make_warc


def spec_bytes(spec):
    p = _prepare_spec(spec)
    return b"".join(p.chunks()), p


class SpecTest(unittest.TestCase):
    def test_spec_bytes_body_lengths(self):
        data, p = spec_bytes(m.RecordSpec("resource", b"hello"))
        self.assertEqual(len(data), len(p.head) + 5 + 4)
        self.assertEqual(p.total, len(data))
        blk = pywarc.WarcReader(io.BytesIO(data), compressed=False).get_next_block()
        self.assertEqual(blk.read(), b"hello")

    def test_spec_file_body_seekable_length_inferred(self):
        f = io.BytesIO(b"xxhelloyy")
        f.seek(2)
        p = _prepare_spec(m.RecordSpec("resource", f))
        self.assertEqual(p.content_length, 7)
        self.assertTrue(p.replayable)
        a = b"".join(p.chunks())
        b = b"".join(p.chunks())
        self.assertEqual(a, b)

    def test_spec_iterable_requires_length(self):
        with self.assertRaises(ValueError):
            _prepare_spec(m.RecordSpec("resource", iter([b"a"])))
        p = _prepare_spec(m.RecordSpec("resource", iter([b"a", b"bc"]), content_length=3))
        self.assertFalse(p.replayable)

    def test_spec_length_mismatch_raises_content_length_mismatch(self):
        for chunks in ([b"a"], [b"abcd"]):
            p = _prepare_spec(m.RecordSpec("resource", iter(chunks), content_length=3))
            with self.assertRaises(pywarc.ContentLengthMismatchError):
                b"".join(p.chunks())
        with self.assertRaises(ValueError):
            _prepare_spec(m.RecordSpec("resource", b"abc", content_length=2))

    def test_spec_head_stable(self):
        s = m.RecordSpec("resource", b"x")
        a = _prepare_spec(s).head
        b = _prepare_spec(s).head
        self.assertEqual(a, b)

    def test_put_body_types(self):
        total, gen, rep, _ = _put_body([m.RecordSpec("resource", b"x"), make_warc(2)])
        self.assertEqual(total, len(b"".join(gen())))
        self.assertTrue(rep)
        with self.assertRaises(TypeError):
            _put_body(["str"])


class RecordStreamTest(unittest.TestCase):
    def setUp(self):
        self.srv = FakeServer().start()
        self.addCleanup(self.srv.stop)
        self.t = Transport(self.srv.url, retries=0)
        self.addCleanup(self.t.close)

    def rec(self, data, path="/r"):
        self.srv.route("GET", path, Resp(200, {"Content-Type": "application/warc"}, data))
        resp = self.t.request("GET", path, stream=True)
        return Record.from_stream(resp, closer=resp.close)

    def test_record_streaming_from_fake_response(self):
        body = b"z" * (5 << 20)
        with self.rec(make_record(0, body=body)) as r:
            chunks = list(r.iter_content())
            self.assertTrue(all(len(c) <= 65536 for c in chunks))
            self.assertEqual(b"".join(chunks), body)
            self.assertEqual(r.warc_type, "resource")
            self.assertEqual(r.target_uri, "http://h0.example/p")
        self.srv.route("GET", "/again", Resp(200, {}, b""))
        self.t.request("GET", "/again")
        self.assertEqual(self.srv.connections, 1)  # fully read: the connection went back to the pool

    def test_record_extra_data_protocol_error(self):
        r = self.rec(make_warc(2))
        with self.assertRaises(m.ProtocolError):
            r.read()

    def test_record_truncated_protocol_error(self):
        r = self.rec(make_record(0, body=b"abcdef" * 50)[:-12])
        with self.assertRaises(m.ProtocolError):
            r.read()

    def test_record_bad_warc_protocol_error(self):
        self.srv.route("GET", "/bad", Resp(200, {}, b"garbage\r\n\r\n"))
        resp = self.t.request("GET", "/bad", stream=True)
        with self.assertRaises(m.ProtocolError):
            Record.from_stream(resp, closer=resp.close)

    def test_record_close_early_closes_connection(self):
        r = self.rec(make_record(0, body=b"q" * (1 << 20)))
        r.read(10)
        r.close()
        self.srv.route("GET", "/n", Resp(200, {}, b""))
        self.t.request("GET", "/n")
        self.assertEqual(self.srv.connections, 2)


if __name__ == "__main__":
    unittest.main()
