"""Immutable data classes for the JSON objects of the Masstuffy HTTP API."""

from __future__ import annotations

import datetime as _dt
from dataclasses import dataclass
from typing import Any, Dict, List, Optional, Tuple, Union

import pywarc

from .errors import ProtocolError

__all__ = [
    "Locator", "LocatorLike", "locator_token", "Codec", "RawRules", "CollectionStats", "CollectionInfo",
    "DeletedCollection", "Limits", "ServerStatus", "DictionaryInfo", "Rejection", "ImportReport", "Page",
    "PutFileReport", "ExportReport",
]

MAX_TOKEN_CHARS = 360_448


def _req(obj: Any, key: str, type_: Any) -> Any:
    if not isinstance(obj, dict):
        raise ProtocolError(f"expected a JSON object, got {type(obj).__name__}")
    if key not in obj:
        raise ProtocolError(f"missing field {key!r}")
    v = obj[key]
    if isinstance(v, bool) and type_ is not bool:
        raise ProtocolError(f"field {key!r} has the wrong type (bool)")
    if not isinstance(v, type_):
        raise ProtocolError(f"field {key!r} has the wrong type ({type(v).__name__})")
    return v


def _opt(obj: Any, key: str, type_: Any) -> Any:
    if not isinstance(obj, dict):
        raise ProtocolError(f"expected a JSON object, got {type(obj).__name__}")
    v = obj.get(key)
    if v is None:
        return None
    return _req(obj, key, type_)


@dataclass(frozen=True)
class Locator:
    """Where a record lives. ``token`` is the opaque value sent back to the server."""

    token: str
    collection: str
    generation: int
    segment: int
    offset: int
    raw_size: int
    digest: str
    date: str          # WARC date as printed by the server (up to 9 fraction digits)
    warc_type: str
    record_id: str
    url: Optional[str]

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "Locator":
        return cls(
            token=_req(obj, "locator", str), collection=_req(obj, "collection", str),
            generation=_req(obj, "generation", int), segment=_req(obj, "segment", int),
            offset=_req(obj, "offset", int), raw_size=_req(obj, "raw_size", int),
            digest=_req(obj, "digest", str), date=_req(obj, "date", str),
            warc_type=_req(obj, "warc_type", str), record_id=_req(obj, "record_id", str),
            url=_opt(obj, "url", str),
        )

    @property
    def datetime(self) -> _dt.datetime:
        """The record date (microsecond precision: extra fraction digits are truncated)."""
        return pywarc.parse_warc_date(self.date)

    def __str__(self) -> str:
        return self.token


LocatorLike = Union[str, Locator]


def locator_token(x: LocatorLike) -> str:
    """Return the token of a Locator or validate a token string (ValueError / TypeError, no request)."""
    if isinstance(x, Locator):
        return x.token
    if not isinstance(x, str):
        raise TypeError(f"locator must be a str or Locator, not {type(x).__name__}")
    if not x or not x.isascii() or not x.startswith("L1") or len(x) > MAX_TOKEN_CHARS:
        raise ValueError("invalid locator token")
    return x


@dataclass(frozen=True)
class Codec:
    """Collection codec: ``none`` or ``zstd`` (with dictionary id and level)."""

    algo: str
    dict_id: Optional[int] = None
    level: Optional[int] = None

    @staticmethod
    def none() -> "Codec":
        return Codec("none")

    @staticmethod
    def zstd(dict_id: int, level: int = 3) -> "Codec":
        if isinstance(dict_id, bool) or not isinstance(dict_id, int) or not 1 <= dict_id <= 2**32 - 1:
            raise ValueError("dict_id must be in 1..2**32-1")
        return Codec("zstd", dict_id, level)

    def to_json(self) -> Dict[str, Any]:
        if self.algo == "zstd":
            return {"algo": "zstd", "dict_id": self.dict_id, "level": self.level}
        return {"algo": self.algo}

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "Codec":
        return cls(_req(obj, "algo", str), _opt(obj, "dict_id", int), _opt(obj, "level", int))


@dataclass(frozen=True)
class RawRules:
    """What the server enforces on payloads of a raw import."""

    payload: str                       # "warc" | "zstd_frame"
    single_frame: bool
    dict_id: Optional[int]
    dictless_min_content_size: int

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "RawRules":
        return cls(_req(obj, "payload", str), _req(obj, "single_frame", bool), _opt(obj, "dict_id", int),
                   _req(obj, "dictless_min_content_size", int))


@dataclass(frozen=True)
class CollectionStats:
    commits: int
    records: int
    cdx_len: int

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "CollectionStats":
        return cls(_req(obj, "commits", int), _req(obj, "records", int), _req(obj, "cdx_len", int))


@dataclass(frozen=True)
class CollectionInfo:
    id: str
    slug: str
    generation: int
    codec: Codec
    raw_rules: RawRules
    stats: Optional[CollectionStats]   # single GET, create and rename only

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "CollectionInfo":
        stats = _opt(obj, "stats", dict)
        return cls(_req(obj, "id", str), _req(obj, "slug", str), _req(obj, "generation", int),
                   Codec.from_json(_req(obj, "codec", dict)), RawRules.from_json(_req(obj, "raw_rules", dict)),
                   None if stats is None else CollectionStats.from_json(stats))


@dataclass(frozen=True)
class DeletedCollection:
    id: str
    slug: str
    trash_removed: Any
    slug_reserved: Any
    index_dropped: Any

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "DeletedCollection":
        return cls(_req(obj, "id", str), _req(obj, "slug", str), obj.get("trash_removed"),
                   obj.get("slug_reserved"), obj.get("index_dropped"))


@dataclass(frozen=True)
class Limits:
    max_put_body_bytes: int
    max_put_records: int
    max_import_body_bytes: Optional[int]
    max_json_body_bytes: int
    max_export_page_bytes: int
    request_timeout_ms: int
    body_idle_timeout_ms: int
    max_concurrent_requests: int

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "Limits":
        return cls(_req(obj, "max_put_body_bytes", int), _req(obj, "max_put_records", int),
                   _opt(obj, "max_import_body_bytes", int), _req(obj, "max_json_body_bytes", int),
                   _req(obj, "max_export_page_bytes", int), _req(obj, "request_timeout_ms", int),
                   _req(obj, "body_idle_timeout_ms", int), _req(obj, "max_concurrent_requests", int))


@dataclass(frozen=True)
class ServerStatus:
    version: str
    api: int
    repository: str
    raw_check: str
    max_record_bytes: int
    collections: int
    jobs_running: int
    dictionaries: int
    limits: Limits
    open_report: Optional[Dict[str, Any]]

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "ServerStatus":
        return cls(_req(obj, "version", str), _req(obj, "api", int), _req(obj, "repository", str),
                   _req(obj, "raw_check", str), _req(obj, "max_record_bytes", int),
                   _req(obj, "collections", int), _req(obj, "jobs_running", int),
                   _req(obj, "dictionaries", int), Limits.from_json(_req(obj, "limits", dict)),
                   _opt(obj, "open_report", dict))


@dataclass(frozen=True)
class DictionaryInfo:
    dict_id: int
    label: str
    size: int

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "DictionaryInfo":
        return cls(_req(obj, "dict_id", int), _req(obj, "label", str), _req(obj, "size", int))


@dataclass(frozen=True)
class Rejection:
    offset: int
    ordinal: int
    record_id: Optional[str]
    error: str

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "Rejection":
        return cls(_req(obj, "offset", int), _req(obj, "ordinal", int), _opt(obj, "record_id", str),
                   _req(obj, "error", str))


@dataclass(frozen=True)
class ImportReport:
    """Outcome of an envelope import.

    ``committed_offset`` is the stream offset to resume from, or ``-1`` when the
    report was adjusted client-side after a codec-change retry restarted the
    stream (resume by record index with ``records`` instead).
    """

    format: Optional[str]
    records: int
    batches: int
    rejected: int
    rejections: List[Rejection]
    committed_offset: int

    @classmethod
    def from_json(cls, obj: Dict[str, Any]) -> "ImportReport":
        rej = _req(obj, "rejections", list)
        return cls(_opt(obj, "format", str), _req(obj, "records", int), _req(obj, "batches", int),
                   _req(obj, "rejected", int), [Rejection.from_json(r) for r in rej],
                   _req(obj, "committed_offset", int))


@dataclass(frozen=True)
class Page:
    items: List[Locator]
    next: Optional[str]


@dataclass(frozen=True)
class PutFileReport:
    records: int
    batches: int
    bytes: int                                   # -1 when unknown (import mode, partial)
    next_record: int
    uncertain: Optional[Tuple[int, int]] = None


@dataclass(frozen=True)
class ExportReport:
    records: int
    unreadable: int
    pages: int
    bytes: int
    cursor: Optional[str]
