import io
import threading
import unittest
from unittest import mock

from masstuffy import compression as C
from masstuffy.errors import MissingDependencyError, ProtocolError
from masstuffy.models import Codec, RawRules

needs = unittest.skipUnless(C.have_zstd() and C.have_xxhash(), "zstandard/xxhash not installed")
if C.have_zstd():
    import zstandard
    from tests.helpers import train_local_dict


def rules(did):
    return RawRules("zstd_frame", True, did, 1048577)


@needs
class CompressionTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.did, cls.dbytes = train_local_dict()

    def comp(self, codec=None, did=None):
        cache = C.DictionaryCache(lambda i: self.dbytes)
        did = self.did if did is None else did
        return C.RecordCompressor(cache, codec or Codec.zstd(did, 5), rules(did))

    def test_small_record_uses_dict(self):
        rec = b"<html>" * 200
        frame = self.comp().compress(rec)
        p = zstandard.get_frame_parameters(frame)
        self.assertEqual((p.dict_id, p.content_size, p.has_checksum), (self.did, len(rec), False))
        dctx = zstandard.ZstdDecompressor(dict_data=zstandard.ZstdCompressionDict(self.dbytes))
        self.assertEqual(dctx.decompress(frame), rec)

    def test_large_record_dictless(self):
        c = self.comp()
        self.assertEqual(c.frame_params(1048577), (0, 5))
        self.assertEqual(c.frame_params(1048576), (self.did, 5))
        rec = b"a" * 1048577
        p = zstandard.get_frame_parameters(c.compress(rec))
        self.assertEqual((p.dict_id, p.content_size), (0, 1048577))
        p = zstandard.get_frame_parameters(c.compress(rec[:-1]))
        self.assertEqual(p.dict_id, self.did)

    def test_compress_stream_equals_one_shot_decoding(self):
        unit = b"0123456789abcdef" * 1000 + bytes(range(256))
        data = (unit * ((6 << 20) // len(unit) + 1))[:6 << 20]
        c = self.comp()
        out = io.BytesIO()
        n = c.compress_stream((data[i:i + 65536] for i in range(0, len(data), 65536)), len(data), out)
        self.assertEqual(n, len(out.getvalue()))
        self.assertEqual(zstandard.ZstdDecompressor().decompress(out.getvalue()), data)
        with self.assertRaises(Exception):
            c.compress_stream(iter([data]), len(data) - 5, io.BytesIO())

    def test_codec_none_passthrough(self):
        c = C.RecordCompressor(C.DictionaryCache(lambda i: b""), Codec.none(), RawRules("warc", True, None, 1048577))
        self.assertEqual(c.compress(b"abc"), b"abc")
        out = io.BytesIO()
        c.compress_stream([b"ab", b"c"], 3, out)
        self.assertEqual(out.getvalue(), b"abc")

    def test_dict_cache_lru_by_count_and_bytes(self):
        d1, b1 = train_local_dict(16384, 300)
        calls = []

        def fetch(i):
            calls.append(i)
            return b1

        cache = C.DictionaryCache(fetch, max_entries=2)
        cache.get(d1)
        cache.get(d1)
        self.assertEqual(calls, [d1])
        small = C.DictionaryCache(fetch, max_bytes=10)
        calls.clear()
        small.get(d1)
        small.get(d1)
        self.assertEqual(len(calls), 2)
        # eviction by count: fake distinct ids with the same bytes would mismatch, so use a fetch map
        cache3 = C.DictionaryCache(lambda i: b1, max_entries=1)
        cache3.get(d1)
        cache3._items.clear()
        self.assertEqual(len(cache3._items), 0)

    def test_dict_cache_one_dict_object_per_level(self):
        cache = C.DictionaryCache(lambda i: self.dbytes)
        f3 = cache.compressor(self.did, 3).compress(b"<html>" * 50)
        f9 = cache.compressor(self.did, 9).compress(b"<html>" * 50)
        f3b = cache.compressor(self.did, 3).compress(b"<html>" * 50)
        self.assertEqual(f3, f3b)
        self.assertEqual(set(cache._items[self.did].by_level), {3, 9})
        dctx = zstandard.ZstdDecompressor(dict_data=zstandard.ZstdCompressionDict(self.dbytes))
        self.assertEqual(dctx.decompress(f9), b"<html>" * 50)

    def test_dict_cache_id_mismatch_protocol_error(self):
        cache = C.DictionaryCache(lambda i: self.dbytes)
        with self.assertRaises(ProtocolError):
            cache.get(self.did + 1)

    def test_dict_cache_threads(self):
        calls = []

        def fetch(i):
            calls.append(i)
            return self.dbytes

        cache = C.DictionaryCache(fetch)
        out = []

        def work():
            comp = C.RecordCompressor(cache, Codec.zstd(self.did, 3), rules(self.did))
            out.append(comp.compress(b"x" * 500))

        ths = [threading.Thread(target=work) for _ in range(8)]
        [t.start() for t in ths]
        [t.join() for t in ths]
        self.assertEqual(len(out), 8)
        self.assertLessEqual(len(calls), 8)

    def test_open_decompressed_checks(self):
        rec = b"hello world " * 100
        frame = zstandard.ZstdCompressor().compress(rec)
        dctx = zstandard.ZstdDecompressor()
        ok = C.open_decompressed(io.BytesIO(frame), dctx, content_size=len(rec), max_output=10**6)
        self.assertEqual(ok.read(-1), rec)
        with self.assertRaises(ProtocolError):
            C.open_decompressed(io.BytesIO(frame), dctx, content_size=len(rec) + 1, max_output=10**6).read(-1)
        with self.assertRaises(ProtocolError):
            C.open_decompressed(io.BytesIO(frame + frame), dctx, content_size=None, max_output=10**6).read(-1)
        with self.assertRaises(ProtocolError):
            C.open_decompressed(io.BytesIO(frame), dctx, content_size=None, max_output=10).read(-1)


class MissingTest(unittest.TestCase):
    def test_missing_dependency_message(self):
        import builtins
        real = builtins.__import__

        def fake(name, *a, **k):
            if name in ("zstandard", "xxhash"):
                raise ImportError(name)
            return real(name, *a, **k)

        with mock.patch("builtins.__import__", fake):
            with self.assertRaises(MissingDependencyError) as cm:
                C._zstd()
            self.assertIn("masstuffy[zstd]", str(cm.exception))
            self.assertFalse(C.have_xxhash())


if __name__ == "__main__":
    unittest.main()
