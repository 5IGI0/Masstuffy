"""Optional client-side zstd compression/decompression (extra ``masstuffy[zstd]``) and xxh3 helpers."""

from __future__ import annotations

import threading
from collections import OrderedDict
from typing import Any, BinaryIO, Callable, Iterable, Optional, Tuple

from .errors import MissingDependencyError, ProtocolError
from .models import Codec, RawRules

__all__ = ["have_zstd", "have_xxhash", "xxh3_hex", "DictionaryCache", "RecordCompressor", "open_decompressed"]


def _zstd() -> Any:
    try:
        import zstandard
    except ImportError as e:
        raise MissingDependencyError("zstandard is required for client-side zstd: pip install masstuffy[zstd]") from e
    return zstandard


def _xxhash() -> Any:
    try:
        import xxhash
    except ImportError as e:
        raise MissingDependencyError("xxhash is required: pip install masstuffy[envelope] (or masstuffy[zstd])") from e
    return xxhash


def have_zstd() -> bool:
    try:
        _zstd()
        return True
    except MissingDependencyError:
        return False


def have_xxhash() -> bool:
    try:
        _xxhash()
        return True
    except MissingDependencyError:
        return False


def xxh3_hex(data: bytes) -> str:
    return "%016x" % _xxhash().xxh3_64_intdigest(data)


class _Entry:
    def __init__(self, data: bytes, d: Any) -> None:
        self.data = data
        self.dict = d                      # for decompression (never precomputed: its CDict is per level)
        self.size = len(data)
        self.by_level: dict = {}           # level -> ZstdCompressionDict with its own precomputed CDict

    def cost(self) -> int:
        return self.size * (1 + len(self.by_level))


class DictionaryCache:
    """Thread-safe LRU of zstd dictionaries keyed by dict_id (immutable server-side, never revalidated).

    Bounded by ``max_entries`` and ``max_bytes``; the byte budget counts the dictionary once per precomputed
    compression level (one dictionary object per (dict_id, level), because a ``ZstdCompressionDict`` holds a
    single precomputed CDict). A dictionary larger than ``max_bytes`` is used but not cached. The fetch
    (network) runs outside the lock.
    """

    def __init__(self, fetch: Callable[[int], bytes], *, max_entries: int = 16, max_bytes: int = 128 << 20) -> None:
        self._fetch = fetch
        self._max_entries = max_entries
        self._max_bytes = max_bytes
        self._lock = threading.Lock()
        self._items: "OrderedDict[int, _Entry]" = OrderedDict()

    def _total(self) -> int:
        return sum(e.cost() for e in self._items.values())

    def _evict(self, keep: Optional[_Entry] = None) -> None:
        while len(self._items) > self._max_entries or self._total() > self._max_bytes:
            victim = next((k for k, e in self._items.items() if e is not keep), None)
            if victim is None:
                return
            del self._items[victim]

    def _entry(self, dict_id: int) -> _Entry:
        if isinstance(dict_id, bool) or not isinstance(dict_id, int) or dict_id < 1:
            raise ValueError("dict_id must be >= 1 (0 means no dictionary)")
        with self._lock:
            e = self._items.get(dict_id)
            if e is not None:
                self._items.move_to_end(dict_id)
                return e
        z = _zstd()
        data = self._fetch(dict_id)
        d = z.ZstdCompressionDict(data)
        if d.dict_id() != dict_id:
            raise ProtocolError(f"dictionary id mismatch: asked {dict_id}, got {d.dict_id()}")
        e = _Entry(data, d)
        if e.size > self._max_bytes:
            return e
        with self._lock:
            if dict_id in self._items:
                return self._items[dict_id]
            self._items[dict_id] = e
            self._evict(keep=e)
        return e

    def get(self, dict_id: int) -> Any:
        return self._entry(dict_id).dict

    def compressor(self, dict_id: int, level: int) -> Any:
        """A new ZstdCompressor (not thread-safe: never share it) writing no checksum, with content size and id."""
        z = _zstd()
        e = self._entry(dict_id)
        with self._lock:
            d = e.by_level.get(level)
            if d is None:
                d = z.ZstdCompressionDict(e.data)
                d.precompute_compress(level=level)
                e.by_level[level] = d
                if e.size <= self._max_bytes and self._items.get(dict_id) is e:
                    self._evict(keep=e)
        return z.ZstdCompressor(level=level, dict_data=d, write_checksum=False, write_content_size=True,
                                write_dict_id=True)

    def decompressor(self, dict_id: int) -> Any:
        z = _zstd()
        if dict_id == 0:
            return z.ZstdDecompressor()
        return z.ZstdDecompressor(dict_data=self.get(dict_id))

    def clear(self) -> None:
        with self._lock:
            self._items.clear()


class RecordCompressor:
    """Compresses whole WARC records to single zstd frames following a collection's ``raw_rules``."""

    def __init__(self, cache: DictionaryCache, codec: Codec, rules: RawRules) -> None:
        self.cache = cache
        self.codec = codec
        self.rules = rules
        if codec.algo == "zstd" and (rules.dict_id is None or codec.level is None):
            raise ProtocolError("zstd collection without an active dictionary in raw_rules")

    def frame_params(self, content_size: int) -> Tuple[int, int]:
        """(dict_id to use, level) for a record of ``content_size`` bytes."""
        if self.codec.algo != "zstd":
            return 0, 0
        assert self.codec.level is not None and self.rules.dict_id is not None  # checked in __init__
        if content_size >= self.rules.dictless_min_content_size:
            return 0, self.codec.level
        return self.rules.dict_id, self.codec.level

    def _cctx(self, content_size: int) -> Tuple[Any, int]:
        did, level = self.frame_params(content_size)
        if did == 0:
            return _zstd().ZstdCompressor(level=level, write_checksum=False, write_content_size=True), 0
        return self.cache.compressor(did, level), did

    @staticmethod
    def _check(prefix: bytes, did: int, content_size: int) -> None:
        p = _zstd().get_frame_parameters(prefix[:32])
        if p.dict_id != did or p.content_size != content_size or p.has_checksum:
            raise RuntimeError("compressed frame does not follow the raw_rules (programming error)")

    def compress(self, record: bytes) -> bytes:
        if self.codec.algo != "zstd":
            return record
        cctx, did = self._cctx(len(record))
        frame = cctx.compress(record)
        self._check(frame, did, len(record))
        return frame

    def compress_stream(self, chunks: Iterable[bytes], content_size: int, out: BinaryIO) -> int:
        """Compress ``chunks`` (total ``content_size``) into ``out`` as one frame. Returns bytes written."""
        if self.codec.algo != "zstd":
            n = 0
            for c in chunks:
                out.write(c)
                n += len(c)
            return n
        cctx, did = self._cctx(content_size)
        z = _zstd()
        counter = _CountingWriter(out)
        with cctx.stream_writer(counter, size=content_size, closefd=False) as w:
            for c in chunks:
                w.write(c)
        self._check(counter.first, did, content_size)
        return counter.count


class _CountingWriter:
    def __init__(self, out: BinaryIO) -> None:
        self._out = out
        self.count = 0
        self.first = b""

    def write(self, b: bytes) -> int:
        if len(self.first) < 32:
            self.first += bytes(b[:32 - len(self.first)])
        self.count += len(b)
        self._out.write(b)
        return len(b)

    def flush(self) -> None:
        pass


class _Decompressed:
    """Reads exactly one zstd frame from ``raw``, in bounded steps (input is fed 1 KiB at a time, which bounds
    the memory spike of a decompression bomb to a few tens of MiB; the total is capped by ``max_output``)."""

    _FEED = 1024

    def __init__(self, raw: Any, dobj: Any, content_size: Optional[int], max_output: int) -> None:
        self._raw = raw
        self._d = dobj
        self._content_size = content_size
        self._max = max_output
        self._count = 0
        self._buf = bytearray()
        self._off = 0
        self._inbuf = b""
        self._eof = False       # frame fully decoded and verified
        self._src_done = False

    def _step(self) -> None:
        zstd = _zstd()
        if self._d.eof:
            self._finish()
            return
        if not self._inbuf:
            self._inbuf = self._raw.read(65536)
            if not self._inbuf:
                raise ProtocolError("truncated zstd frame")
        piece, self._inbuf = self._inbuf[:self._FEED], self._inbuf[self._FEED:]
        try:
            out = self._d.decompress(piece)
        except zstd.ZstdError as e:
            raise ProtocolError(f"invalid zstd data: {e}") from e
        self._count += len(out)
        if self._count > self._max:
            raise ProtocolError("decompressed size exceeds limit")
        self._buf += out
        if self._d.eof:
            self._finish(extra=self._inbuf)

    def _finish(self, extra: bytes = b"") -> None:
        if self._eof:
            return
        if getattr(self._d, "unused_data", b"") or extra or self._raw.read(1) != b"":
            raise ProtocolError("data after the zstd frame (exactly one frame expected)")
        if self._content_size is not None and self._count != self._content_size:
            raise ProtocolError("decompressed size differs from the declared content size")
        self._eof = True

    def read(self, n: int = -1) -> bytes:
        want = None if n is None or n < 0 else n
        while not self._eof and (want is None or len(self._buf) - self._off < want):
            self._step()
        end = len(self._buf) if want is None else min(len(self._buf), self._off + want)
        data = bytes(self._buf[self._off:end])
        self._off = end
        if self._off >= len(self._buf):
            self._buf.clear()
            self._off = 0
        elif self._off > (1 << 20):
            del self._buf[:self._off]   # compact occasionally: slicing on every read would be superlinear
            self._off = 0
        return data

    def close(self) -> None:
        self._eof = True


def open_decompressed(raw: Any, dctx: Any, *, content_size: Optional[int], max_output: int) -> Any:
    """Wrap ``raw`` (a stream of exactly one zstd frame) in a reader of the decompressed bytes.

    ProtocolError on invalid data, a second frame or trailing bytes, a size differing from ``content_size``,
    or output above ``max_output``.
    """
    if content_size is not None and content_size > max_output:
        raise ProtocolError("declared content size exceeds limit")
    return _Decompressed(raw, dctx.decompressobj(read_across_frames=False), content_size, max_output)
