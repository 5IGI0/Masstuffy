"""Records: building them client-side (RecordSpec), and reading them from responses (Record, RawBody)."""

from __future__ import annotations

import os
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import (Any, BinaryIO, Callable, Iterable, Iterator, List, Mapping, Optional, Sequence, Tuple,
                    Union)

import pywarc

from .errors import ProtocolError

__all__ = ["RecordSpec", "Record", "RawBody"]

CHUNK = 65536
TRAILER = b"\r\n\r\n"
_BYTES = (bytes, bytearray, memoryview)


@dataclass
class RecordSpec:
    """A record to build client-side (always WARC/1.1, via ``pywarc.serialize_record_head``).

    ``body`` is bytes-like, a binary file object or an iterable of bytes. ``content_length`` is required for
    iterables and for non-seekable files. ``record_id`` / ``date`` default to a fresh uuid / now (fixed when the
    spec is first prepared, so retries send the same record).
    """

    warc_type: str
    body: Union[bytes, bytearray, memoryview, BinaryIO, Iterable[bytes]] = b""
    content_length: Optional[int] = None
    record_id: Optional[str] = None
    date: Optional[datetime] = None
    headers: Optional[Mapping[str, Union[str, Sequence[str], None]]] = None
    warcinfo_id: Optional[str] = None


@dataclass
class _Prepared:
    head: bytes
    content_length: int
    chunks: Callable[[], Iterator[bytes]]
    replayable: bool

    @property
    def total(self) -> int:
        return len(self.head) + self.content_length + len(TRAILER)


def _is_file(body: Any) -> bool:
    return hasattr(body, "read") and not isinstance(body, _BYTES)


def _file_remaining(fp: Any) -> Optional[int]:
    try:
        if not fp.seekable():
            return None
        pos = fp.tell()
        end = fp.seek(0, os.SEEK_END)
        fp.seek(pos)
        return end - pos
    except (AttributeError, OSError, ValueError):
        return None


def _prepare_spec(spec: RecordSpec) -> _Prepared:
    if spec.record_id is None:
        spec.record_id = uuid.uuid4().urn
    if spec.date is None:
        spec.date = datetime.now(timezone.utc)
    body = spec.body
    given = spec.content_length
    replayable = False
    if isinstance(body, _BYTES):
        n = memoryview(body).nbytes
        if given is not None and given != n:
            raise ValueError("content_length does not match the body length")
        cl = n
        replayable = True
    elif _is_file(body):
        if given is not None:
            cl = given
        else:
            rem = _file_remaining(body)
            if rem is None:
                raise ValueError("content_length required for a non-seekable file body")
            cl = rem
        try:
            replayable = bool(body.seekable())
        except (AttributeError, OSError, ValueError):
            replayable = False
    else:
        if isinstance(body, str):
            raise TypeError("record body must be bytes, a binary file or an iterable of bytes")
        if given is None:
            raise ValueError("content_length required for an iterable body")
        cl = given
    head = pywarc.serialize_record_head(spec.warc_type, cl, record_id=spec.record_id, record_date=spec.date,
                                        record_headers=spec.headers, warcinfo_id=spec.warcinfo_id)
    start = body.tell() if _is_file(body) and replayable else None

    def chunks() -> Iterator[bytes]:
        yield head
        count = 0
        if isinstance(body, _BYTES):
            if cl:
                yield bytes(body) if not isinstance(body, bytes) else body
            count = cl
        else:
            if start is not None:
                body.seek(start)
            if _is_file(body):
                src: Iterable[bytes] = iter(lambda: body.read(CHUNK), b"")
            else:
                src = body  # type: ignore[assignment]
            for c in src:
                if not c:
                    continue
                count += len(c)
                if count > cl:
                    raise pywarc.ContentLengthMismatchError("record body is longer than its content_length")
                yield bytes(c)
        if count != cl:
            raise pywarc.ContentLengthMismatchError("record body is shorter than its content_length")
        yield TRAILER

    return _Prepared(head, cl, chunks, replayable)


def _put_body(items: Sequence[Any]) -> Tuple[int, Callable[[], Iterator[bytes]], bool, List[Optional[_Prepared]]]:
    """Prepare put items: returns (total_length, chunk generator factory, replayable, prepared list)."""
    prepared: List[Optional[_Prepared]] = []
    total = 0
    replayable = True
    for it in items:
        if isinstance(it, RecordSpec):
            p = _prepare_spec(it)
            prepared.append(p)
            total += p.total
            replayable = replayable and p.replayable
        elif isinstance(it, _BYTES):
            prepared.append(None)
            total += memoryview(it).nbytes
        else:
            raise TypeError("put items must be RecordSpec or bytes-like WARC data, not %s" % type(it).__name__)

    def gen() -> Iterator[bytes]:
        for it, p in zip(items, prepared):
            if p is not None:
                yield from p.chunks()
            elif memoryview(it).nbytes:
                yield bytes(it)

    return total, gen, replayable, prepared


class Record:
    """A record read from the server, parsed by pywarc while it streams.

    Valid until closed, or until the owning iterator advances. Use as a context manager. Reading the whole body
    of a single-record response also validates the trailer and that nothing follows.
    """

    def __init__(self, block: "pywarc.WarcBlock", *, locator: Optional[str] = None,
                 reader: Optional["pywarc.WarcReader"] = None,
                 closer: Optional[Callable[[], None]] = None) -> None:
        self.block = block
        self.locator = locator
        self._reader = reader        # set => single-record stream: check the end when the body is read
        self._closer = closer
        self._finished = reader is None
        self._closed = False
        self._maybe_finish()

    @classmethod
    def from_stream(cls, stream: Any, *, locator: Optional[str] = None, compressed: bool = False,
                    closer: Optional[Callable[[], None]] = None) -> "Record":
        """Parse the single record of a (non-seekable) stream. ``closer`` is called by close()."""
        try:
            reader = pywarc.WarcReader(stream, compressed=compressed)
            block = reader.get_next_block()
        except pywarc.InvalidWarcError as e:
            if closer:
                closer()
            raise ProtocolError(f"malformed WARC in response: {e}") from e
        except BaseException:
            if closer:
                closer()
            raise
        if block is None:
            if closer:
                closer()
            raise ProtocolError("empty response body, expected one WARC record")
        return cls(block, locator=locator, reader=reader, closer=closer)

    # -- metadata (delegated to pywarc)

    @property
    def version(self) -> str:
        return self.block.version

    @property
    def headers(self):
        return self.block.headers

    def header(self, name: str, default: Optional[str] = None) -> Optional[str]:
        return self.block.header(name, default)

    def header_all(self, name: str) -> List[str]:
        return self.block.header_all(name)

    warc_type = property(lambda self: self.block.type)
    record_id = property(lambda self: self.block.record_id)
    date = property(lambda self: self.block.date)
    target_uri = property(lambda self: self.block.target_uri)
    content_type = property(lambda self: self.block.content_type)
    content_length = property(lambda self: self.block.content_length)
    payload_digest = property(lambda self: self.block.payload_digest)
    block_digest = property(lambda self: self.block.block_digest)

    # -- body

    def _maybe_finish(self) -> None:
        if self._finished or self.block.read_offset < self.block.content_length:
            return
        self._finished = True
        try:
            nxt = self._reader.get_next_block()  # type: ignore[union-attr]
        except pywarc.InvalidWarcError as e:
            self.close()
            raise ProtocolError(f"malformed WARC in response: {e}") from e
        except BaseException:
            self.close()
            raise
        if nxt is not None:
            self.close()
            raise ProtocolError("extra data after record")
        self.close()

    def read(self, n: int = -1) -> bytes:
        """Read up to ``n`` body bytes; ``n < 0`` reads the rest (slurps it into memory)."""
        if self._closed and self.block.read_offset < self.block.content_length:
            raise ValueError("read from a closed record")
        try:
            data = self.block.read(None if n is None or n < 0 else n)
        except pywarc.InvalidWarcError as e:
            self.close()
            raise ProtocolError(f"malformed or truncated WARC in response: {e}") from e
        except BaseException:
            self.close()
            raise
        self._maybe_finish()
        return data

    def iter_content(self, chunk_size: int = CHUNK) -> Iterator[bytes]:
        """Body chunks of at most ``chunk_size`` bytes (never empty)."""
        while self.block.read_offset < self.block.content_length:
            chunk = self.read(chunk_size)
            if not chunk:
                break
            yield chunk

    def copy_to(self, fp: BinaryIO, chunk_size: int = CHUNK) -> int:
        """Write the remaining body to ``fp``; returns the byte count."""
        total = 0
        for c in self.iter_content(chunk_size):
            fp.write(c)
            total += len(c)
        return total

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self._closer is not None:
            self._closer()

    def __enter__(self) -> "Record":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()


class RawBody:
    """A ``/raw`` response body: the stored bytes, or decompressed locally (``encoding == "warc"``).

    ``raw_size`` and ``digest`` always describe the stored bytes (``Masstuffy-Raw-Size`` /
    ``Masstuffy-Digest``). Use as a context manager.
    """

    def __init__(self, stream: Any, *, encoding: str, dict_id: Optional[int], raw_size: int, digest: str,
                 content_size: Optional[int], closer: Callable[[], None]) -> None:
        self._stream = stream
        self.encoding = encoding
        self.dict_id = dict_id
        self.raw_size = raw_size
        self.digest = digest
        self.content_size = content_size
        self._closer = closer
        self._closed = False
        self._buf = b""

    def read(self, n: int = -1) -> bytes:
        """Read up to ``n`` bytes; ``n < 0`` reads everything (slurps)."""
        if self._closed:
            return b""
        if n is None or n < 0:
            data, self._buf = self._buf + self._stream.read(-1), b""
            return data
        if self._buf:
            data, self._buf = self._buf[:n], self._buf[n:]
            return data
        return self._stream.read(n)

    def readline(self, limit: int = -1) -> bytes:
        """Read one line (file protocol, used by pywarc to parse a decoded record)."""
        if self._closed:
            return b""
        while b"\n" not in self._buf and (limit is None or limit < 0 or len(self._buf) < limit):
            more = self._stream.read(4096)
            if not more:
                break
            self._buf += more
        i = self._buf.find(b"\n")
        end = len(self._buf) if i < 0 else i + 1
        if limit is not None and 0 <= limit < end:
            end = limit
        line, self._buf = self._buf[:end], self._buf[end:]
        return line

    def readable(self) -> bool:
        return True

    def seekable(self) -> bool:
        return False

    def iter_content(self, chunk_size: int = CHUNK) -> Iterator[bytes]:
        while True:
            c = self.read(chunk_size)
            if not c:
                return
            yield c

    def copy_to(self, fp: BinaryIO, chunk_size: int = CHUNK) -> int:
        total = 0
        for c in self.iter_content(chunk_size):
            fp.write(c)
            total += len(c)
        return total

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._closer()

    def __enter__(self) -> "RawBody":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()
