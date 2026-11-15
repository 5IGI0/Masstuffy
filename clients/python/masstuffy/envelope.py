"""Writer of V1 raw-import envelopes (needs xxhash).

The metadata of an envelope line is compared by the server with the record itself, so the helpers here
follow the engine's own parsing rules exactly (see docs/http-api.md, section 6).
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import BinaryIO, Optional

from .compression import _xxhash

__all__ = ["ENVELOPE_HEADER", "MAX_LINE", "EnvelopeWriter", "EnvelopeMeta", "canonical_warc_date", "escape_url",
           "v1_line", "meta_from_head"]

ENVELOPE_HEADER = b"#masstuffy-cdx/1 gen=0\n"
MAX_LINE = 256 * 1024
_DATE_RE = re.compile(r"(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?Z")


def canonical_warc_date(raw: str) -> str:
    """The engine's canonical date: strict ``YYYY-MM-DDThh:mm:ss[.f]Z``, fraction trailing zeros trimmed.

    Raises ValueError for anything the engine would reject (offsets, date-only forms, no ``Z``).
    """
    m = _DATE_RE.fullmatch(raw)
    if m is None:
        raise ValueError(f"engine would reject WARC-Date {raw!r}")
    y, mo, d, h, mi, s, frac = m.groups()
    if not (1 <= int(mo) <= 12 and 1 <= int(d) <= 31 and int(h) <= 23 and int(mi) <= 59 and int(s) <= 60):
        raise ValueError(f"WARC-Date out of range: {raw!r}")
    out = f"{y}-{mo}-{d}T{h}:{mi}:{s}"
    if frac:
        frac = frac.rstrip("0")
        if frac:
            out += "." + frac
    return out + "Z"


def escape_url(url: Optional[str]) -> str:
    """Envelope URL field: ``-`` for absent or empty, ``%2D`` for a literal ``-``, ``%XX`` for bytes <= 0x20, ``%``, 0x7F."""
    if url is None or url == "":
        return "-"
    if url == "-":
        return "%2D"
    out = bytearray()
    for b in url.encode("utf-8"):
        if b <= 0x20 or b == 0x25 or b == 0x7F:
            out += b"%%%02X" % b
        else:
            out.append(b)
    return out.decode("utf-8")


def _check_token(name: str, v: str) -> None:
    if not v or any(b <= 0x20 or b == 0x7F for b in v.encode("utf-8")):
        raise ValueError(f"invalid {name} for an envelope line: {v!r}")


def v1_line(raw_size: int, digest: int, date: str, warc_type: str, record_id: str, url: Optional[str]) -> bytes:
    """One V1 envelope line (without its payload), including the check field and the newline."""
    if raw_size < 1:
        raise ValueError("raw_size must be >= 1")
    _check_token("warc_type", warc_type)
    _check_token("record_id", record_id)
    prefix = f"A 0 0 {raw_size} {digest:016x} {date} {warc_type} {record_id} {escape_url(url)}".encode("utf-8")
    check = _xxhash().xxh3_64_intdigest(prefix) & 0xFFFFFFFF
    line = prefix + b" " + b"%08x" % check + b"\n"
    if len(line) > MAX_LINE:
        raise ValueError("envelope line too long")
    return line


@dataclass(frozen=True)
class EnvelopeMeta:
    warc_type: str
    record_id: str
    date: str
    url: Optional[str]


def meta_from_head(head: bytes) -> EnvelopeMeta:
    """Envelope metadata of a raw record head, parsed like the engine (verbatim values, split at ``": "``)."""
    text = head.decode("utf-8")
    lines = text.split("\r\n")
    version = lines[0]
    found: dict = {}
    for line in lines[1:]:
        if line == "":
            break
        name, sep, value = line.partition(": ")
        if not sep:
            raise ValueError("engine would reject: malformed header line")
        found.setdefault(name.lower(), value)
    for need in ("warc-type", "warc-record-id", "warc-date"):
        if need not in found:
            raise ValueError(f"missing {need} header")
    rid = found["warc-record-id"]
    if not (rid.startswith("<") and rid.endswith(">") and len(rid) >= 2):
        raise ValueError("WARC-Record-ID must be enclosed in <>")
    url = found.get("warc-target-uri")
    if url is not None and version == "WARC/1.0" and url.startswith("<") and url.endswith(">") and len(url) >= 2:
        url = url[1:-1]
    return EnvelopeMeta(found["warc-type"], rid[1:-1], canonical_warc_date(found["warc-date"]), url or None)


class EnvelopeWriter:
    """Builds a V1 envelope file: header on creation, then ``add`` / ``add_spooled`` per record."""

    def __init__(self, fp: BinaryIO) -> None:
        self._fp = fp
        fp.write(ENVELOPE_HEADER)

    def add(self, payload: bytes, meta: EnvelopeMeta) -> None:
        digest = _xxhash().xxh3_64_intdigest(payload)
        self._fp.write(v1_line(len(payload), digest, meta.date, meta.warc_type, meta.record_id, meta.url))
        self._fp.write(payload)

    def add_spooled(self, spool: BinaryIO, size: int, digest: int, meta: EnvelopeMeta) -> None:
        self._fp.write(v1_line(size, digest, meta.date, meta.warc_type, meta.record_id, meta.url))
        spool.seek(0)
        left = size
        while left > 0:
            c = spool.read(min(65536, left))
            if not c:
                raise ValueError("spool shorter than declared size")
            self._fp.write(c)
            left -= len(c)
