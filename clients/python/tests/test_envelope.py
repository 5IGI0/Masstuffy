import io
import unittest

from masstuffy.compression import have_xxhash
from masstuffy.envelope import (ENVELOPE_HEADER, EnvelopeMeta, EnvelopeWriter, canonical_warc_date, escape_url,
                                meta_from_head, v1_line)

needs_xx = unittest.skipUnless(have_xxhash(), "xxhash not installed")


class EnvelopeTest(unittest.TestCase):
    def test_canonical_date_strict(self):
        c = canonical_warc_date
        self.assertEqual(c("2020-01-02T03:04:05Z"), "2020-01-02T03:04:05Z")
        self.assertEqual(c("2020-01-02T03:04:05.500Z"), "2020-01-02T03:04:05.5Z")
        self.assertEqual(c("2020-01-02T03:04:05.000Z"), "2020-01-02T03:04:05Z")
        self.assertEqual(c("2020-01-02T03:04:05.123456789Z"), "2020-01-02T03:04:05.123456789Z")
        self.assertEqual(c("2020-01-02T03:04:60Z"), "2020-01-02T03:04:60Z")
        for bad in ("2020-01-02T03:04:05+01:00", "2020", "2020-01-02T03:04:05", "2020-01-02", "2020-13-02T03:04:05Z",
                    "2020-01-02T03:04:05.1234567890Z"):
            with self.assertRaises(ValueError):
                c(bad)

    def test_escape_url(self):
        self.assertEqual(escape_url(None), "-")
        self.assertEqual(escape_url(""), "-")
        self.assertEqual(escape_url("-"), "%2D")
        self.assertEqual(escape_url("a b%c\x7f\x01"), "a%20b%25c%7F%01")
        self.assertEqual(escape_url("http://é/"), "http://é/")

    @needs_xx
    def test_v1_line_layout(self):
        import xxhash
        self.assertEqual(xxhash.xxh3_64_intdigest(b""), 0x2D06800538D394C2)
        line = v1_line(10, 0xAB, "2020-01-02T03:04:05Z", "response", "urn:uuid:1", "http://a/ b")
        self.assertTrue(line.endswith(b"\n"))
        prefix, check = line[:-1].rsplit(b" ", 1)
        self.assertEqual(check, b"%08x" % (xxhash.xxh3_64_intdigest(prefix) & 0xFFFFFFFF))
        self.assertEqual(prefix.split(b" "), [b"A", b"0", b"0", b"10", b"00000000000000ab",
                                              b"2020-01-02T03:04:05Z", b"response", b"urn:uuid:1", b"http://a/%20b"])

    @needs_xx
    def test_v1_line_rejects(self):
        d = "2020-01-02T03:04:05Z"
        for args in ((0, 1, d, "t", "id", None), (5, 1, d, "t", "a b", None), (5, 1, d, "t", "id", "u" * 300000)):
            with self.assertRaises(ValueError):
                v1_line(*args)

    def test_meta_from_head_verbatim(self):
        head = (b"WARC/1.1\r\nWARC-Type: response\r\nWARC-Record-ID: <urn:uuid:1>\r\n"
                b"WARC-Date: 2020-01-02T03:04:05Z\r\nWARC-Target-URI: http://x/ \r\nContent-Length: 0\r\n\r\n")
        self.assertEqual(meta_from_head(head), EnvelopeMeta("response", "urn:uuid:1", "2020-01-02T03:04:05Z",
                                                            "http://x/ "))
        bad = head.replace(b"WARC-Type: response", b"WARC-Type:response")
        with self.assertRaises(ValueError):
            meta_from_head(bad)

    def test_meta_1_0_brackets(self):
        def head(v, uri):
            return (v + b"\r\nWARC-Type: r\r\nWARC-Record-ID: <i>\r\nWARC-Date: 2020-01-02T03:04:05Z\r\n"
                    b"WARC-Target-URI: " + uri + b"\r\nContent-Length: 0\r\n\r\n")

        self.assertEqual(meta_from_head(head(b"WARC/1.0", b"<http://x/>")).url, "http://x/")
        self.assertEqual(meta_from_head(head(b"WARC/1.0", b"<http://x/")).url, "<http://x/")
        self.assertEqual(meta_from_head(head(b"WARC/1.1", b"<http://x/>")).url, "<http://x/>")

    def test_empty_url_is_none(self):
        head = (b"WARC/1.1\r\nWARC-Type: r\r\nWARC-Record-ID: <i>\r\nWARC-Date: 2020-01-02T03:04:05Z\r\n"
                b"WARC-Target-URI: \r\nContent-Length: 0\r\n\r\n")
        self.assertIsNone(meta_from_head(head).url)

    @needs_xx
    def test_envelope_writer_bytes(self):
        out = io.BytesIO()
        w = EnvelopeWriter(out)
        meta = EnvelopeMeta("resource", "urn:uuid:1", "2020-01-02T03:04:05Z", None)
        w.add(b"PAYLOAD", meta)
        import xxhash
        exp = ENVELOPE_HEADER + v1_line(7, xxhash.xxh3_64_intdigest(b"PAYLOAD"), meta.date, "resource",
                                        "urn:uuid:1", None) + b"PAYLOAD"
        self.assertEqual(out.getvalue(), exp)


if __name__ == "__main__":
    unittest.main()
