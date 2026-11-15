"""Raw WARC record framing over pywarc: the exact bytes of every record, streamed.

Masstuffy stores records byte-exactly, so uploads and exports must forward the source bytes, never a
re-serialization. pywarc >= 0.4.1 exposes the exact head bytes as ``WarcBlock.raw_head``.
"""

from __future__ import annotations

import io
import os
from contextlib import contextmanager
from dataclasses import dataclass
from typing import BinaryIO, Iterator, Optional, Tuple, Union

import pywarc

from .errors import MissingDependencyError

__all__ = ["RawRecord", "iter_raw_records", "open_warc_source", "TRAILER"]

TRAILER = b"\r\n\r\n"

# Class-level feature detection (raw_head itself is an instance attribute): iter_raw came with raw_head.
_HAS_RAW_HEAD = hasattr(pywarc.WarcBlock, "iter_raw")

_NEED = "pywarc >= 0.4.1 is required (WarcBlock.raw_head)"


@dataclass
class RawRecord:
    """One record of a WARC stream. Valid only until the iterator advances."""

    ordinal: int               # 0-based index in the stream
    offset: int                # uncompressed offset of the version line
    head: bytes                # exact bytes: version line .. blank line, inclusive
    block: "pywarc.WarcBlock"  # body readable via block.read(n) until content_length

    @property
    def size(self) -> int:
        """Size of the whole record: head + body + 4-byte trailer."""
        return len(self.head) + self.block.content_length + len(TRAILER)

    def iter_body(self, chunk_size: int = 65536) -> Iterator[bytes]:
        """Body chunks (never empty). Raises pywarc.TruncatedWarcError if the input ends early."""
        remaining = self.block.content_length - self.block.read_offset
        while remaining > 0:
            chunk = self.block.read(min(chunk_size, remaining))
            if not chunk:
                break
            remaining -= len(chunk)
            yield chunk

    def iter_raw(self, chunk_size: int = 65536) -> Iterator[bytes]:
        """The record byte-exactly: head, body chunks, canonical trailer."""
        yield self.head
        yield from self.iter_body(chunk_size)
        yield TRAILER


def iter_raw_records(stream: BinaryIO, *, compressed: bool = False, max_header_bytes: int = 65536,
                     max_headers: int = 256, start: int = 0) -> Iterator[RawRecord]:
    """Iterate the records of a WARC stream (plain, or gzip with ``compressed=True``).

    Records before ``start`` are skipped (their bodies are not read). A yielded record is valid only until the
    next iteration (pywarc semantics). pywarc errors (InvalidWarcError, TruncatedWarcError) propagate.
    Raises MissingDependencyError (immediately) when pywarc is too old.
    """
    if not _HAS_RAW_HEAD:
        raise MissingDependencyError(_NEED)
    return _iter(stream, compressed, max_header_bytes, max_headers, start)


def _iter(stream, compressed, max_header_bytes, max_headers, start) -> Iterator[RawRecord]:
    reader = pywarc.WarcReader(stream, compressed=compressed, max_header_bytes=max_header_bytes,
                               max_headers=max_headers)
    try:
        ordinal = 0
        while True:
            blk = reader.get_next_block()
            if blk is None:
                return
            head = blk.raw_head
            if not isinstance(head, (bytes, bytearray)):
                raise MissingDependencyError(_NEED)
            if ordinal >= start:
                yield RawRecord(ordinal, blk.block_content_pos - len(head), bytes(head), blk)
            ordinal += 1
    finally:
        reader.close()


class _Peekable:
    """Minimal wrapper giving a non-seekable stream a 2-byte lookahead."""

    def __init__(self, raw) -> None:
        self._raw = raw
        self._buf = b""

    def sniff(self, n: int) -> bytes:
        while len(self._buf) < n:
            more = self._raw.read(n - len(self._buf))
            if not more:
                break
            self._buf += more
        return self._buf[:n]

    def read(self, n: int = -1) -> bytes:
        if n is None or n < 0:
            data, self._buf = self._buf + self._raw.read(), b""
            return data
        if self._buf:
            data, self._buf = self._buf[:n], self._buf[n:]
            if len(data) < n:
                data += self._raw.read(n - len(data))
            return data
        return self._raw.read(n)

    def readline(self, limit: int = -1) -> bytes:
        if self._buf:
            i = self._buf.find(b"\n")
            end = len(self._buf) if i < 0 else i + 1
            if limit is not None and 0 <= limit < end:
                end = limit
            line, self._buf = self._buf[:end], self._buf[end:]
            if line.endswith(b"\n") or (limit is not None and 0 <= limit <= len(line)):
                return line
            more = self._raw.readline(-1 if limit is None or limit < 0 else limit - len(line))
            return line + more
        return self._raw.readline(limit)

    def seekable(self) -> bool:
        return False

    def readable(self) -> bool:
        return True


@contextmanager
def open_warc_source(src: Union[str, "os.PathLike[str]", BinaryIO],
                     compressed: Optional[bool] = None) -> Iterator[Tuple[BinaryIO, bool]]:
    """Open a WARC source and yield ``(stream, compressed)`` for ``iter_raw_records``.

    A path is opened and closed at exit; a file object is never closed. ``compressed=None`` sniffs the gzip
    magic number (multi-member gzip, such as one member per record, is supported by pywarc).
    """
    opened = None
    try:
        if isinstance(src, (str, os.PathLike)):
            opened = open(src, "rb")
            stream: BinaryIO = opened
        else:
            stream = src
        if compressed is None:
            magic = b""
            try:
                seekable = bool(stream.seekable())
            except AttributeError:
                seekable = False
            if seekable:
                pos = stream.tell()
                magic = stream.read(2)
                stream.seek(pos)
            elif hasattr(stream, "peek"):
                magic = stream.peek(2)[:2]
            else:
                stream = _Peekable(stream)  # type: ignore[assignment]
                magic = stream.sniff(2)  # type: ignore[attr-defined]
            compressed = magic == b"\x1f\x8b"
        yield stream, bool(compressed)
    finally:
        if opened is not None:
            opened.close()
