import gzip
import io
import os
import tempfile
import unittest
from unittest import mock

import pywarc

from masstuffy import _framing
from masstuffy._framing import iter_raw_records, open_warc_source
from masstuffy.errors import MissingDependencyError
from tests.helpers import make_record, make_warc


def wr(path, data):
    with open(path, "wb") as f:
        f.write(data)


def all_raw(stream, **kw):
    return [bytes().join(r.iter_raw()) for r in iter_raw_records(stream, **kw)]


class FramingTest(unittest.TestCase):
    def test_raw_head_exact_bytes(self):
        odd = (b"WARC/1.0\r\nWARC-Type:resource\r\nWARC-Record-ID:   <urn:uuid:1>\r\nWARC-Date: 2020-01-02T03:04:05Z\r\n"
               b"X-Fold: a\r\n   b\r\nContent-Length:  5\r\n\r\nhello\r\n\r\n")
        data = make_record(0, version=b"WARC/1.0") + odd + make_record(2)
        out = b"".join(all_raw(io.BytesIO(data)))
        self.assertEqual(out, data)
        heads = [r.head for r in iter_raw_records(io.BytesIO(data))]
        self.assertTrue(heads[1].startswith(b"WARC/1.0\r\nWARC-Type:resource"))
        self.assertIn(b"X-Fold: a\r\n   b\r\n", heads[1])

    def test_offsets_and_ordinals(self):
        data = make_warc(4)
        recs = [(r.ordinal, r.offset, r.size) for r in iter_raw_records(io.BytesIO(data))]
        self.assertEqual([x[0] for x in recs], [0, 1, 2, 3])
        pos = 0
        for _, off, size in recs:
            self.assertEqual(off, pos)
            pos += size
        self.assertEqual(pos, len(data))

    def test_gzip_multi_member_source(self):
        plain = make_warc(5)
        members = b"".join(gzip.compress(make_record(i)) for i in range(5))
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "x.warc.gz")
            wr(p, members)
            with open_warc_source(p, None) as (s, comp):
                self.assertTrue(comp)
                self.assertEqual(b"".join(all_raw(s, compressed=comp)), plain)

    def test_gzip_single_member_source(self):
        plain = make_warc(5)
        bio = io.BytesIO(gzip.compress(plain))
        with open_warc_source(bio, None) as (s, comp):
            self.assertEqual(b"".join(all_raw(s, compressed=comp)), plain)
        self.assertFalse(bio.closed)

    def test_sniff_without_suffix_and_forced_false(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "noext")
            wr(p, gzip.compress(make_warc(2)))
            with open_warc_source(p, None) as (s, comp):
                self.assertTrue(comp)
            with open_warc_source(p, False) as (s, comp):
                self.assertFalse(comp)
            q = os.path.join(d, "plain.gz")
            wr(q, make_warc(2))
            with open_warc_source(q, None) as (s, comp):
                self.assertFalse(comp)

    def test_non_seekable_source(self):
        class Pipe:
            def __init__(self, b):
                self.b = io.BytesIO(b)

            def read(self, n=-1):
                return self.b.read(n)

            def readline(self, n=-1):
                return self.b.readline(n)

        for payload in (make_warc(3), gzip.compress(make_warc(3))):
            with open_warc_source(Pipe(payload), None) as (s, comp):
                self.assertEqual(b"".join(all_raw(s, compressed=comp)), make_warc(3))

    def test_start_skips_records(self):
        data = make_warc(5)
        got = []
        for r in iter_raw_records(io.BytesIO(data), start=2):
            got.append((r.ordinal, r.offset))
        full = [r.offset for r in iter_raw_records(io.BytesIO(data))]
        self.assertEqual(got, [(2, full[2]), (3, full[3]), (4, full[4])])

    def test_truncated_source_raises_truncated(self):
        data = make_warc(2)[:-10]
        with self.assertRaises(pywarc.TruncatedWarcError):
            all_raw(io.BytesIO(data))

    def test_bad_trailer_raises(self):
        data = make_record(0)[:-4] + b"XXXX" + make_record(1)
        with self.assertRaises(pywarc.InvalidWarcError):
            all_raw(io.BytesIO(data))

    def test_limits_256_headers(self):
        extra = b"".join(b"X-H%d: v\r\n" % i for i in range(257))
        data = b"WARC/1.1\r\nWARC-Type: resource\r\n" + extra + b"Content-Length: 0\r\n\r\n\r\n\r\n"
        with self.assertRaises(pywarc.InvalidWarcError):
            all_raw(io.BytesIO(data))

    def test_old_pywarc_detected(self):
        with mock.patch.object(_framing, "_HAS_RAW_HEAD", False):
            with self.assertRaises(MissingDependencyError):
                iter_raw_records(io.BytesIO(make_warc(1)))


if __name__ == "__main__":
    unittest.main()
