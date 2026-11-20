"""The Masstuffy client: Client, Collection and Job."""

from __future__ import annotations

import dataclasses
import gzip
import io
import json
import math
import os
import ssl
import tempfile
import threading
import time
from datetime import datetime
from typing import (Any, BinaryIO, Callable, Dict, Iterable, Iterator, List, Mapping, Optional, Sequence, Tuple,
                    Union)

import pywarc

from . import _framing
from ._framing import RawRecord, iter_raw_records, open_warc_source
from ._transport import Response, StreamingResponse, Transport, seg
from .compression import (DictionaryCache, RecordCompressor, _xxhash, _zstd, have_xxhash, open_decompressed)
from .envelope import ENVELOPE_HEADER, MAX_LINE, meta_from_head, v1_line
from .errors import (ApiError, ExportInterruptedError, GrepIncompleteError, JobFailedError, JobTimeoutError, MasstuffyError,
                     MissingDependencyError, NotFoundError, PartialPutError, ProtocolError, RecordTooLargeError,
                     ServerError, TransportError, UnavailableError, error_from_object)
from .models import (Codec, CollectionInfo, DeletedCollection, DictionaryInfo, ExportReport, GrepHit,
                     GrepProgress, GrepQuery, GrepReport, GrepUnreadable, ImportReport, Locator, LocatorLike, Page, PutFileReport, ServerStatus, locator_token)
from .records import RawBody, Record, RecordSpec, _put_body, _Prepared, _prepare_spec

__all__ = ["Client", "Collection", "Job", "GrepStream"]

CHUNK = 65536
SPOOL_THRESHOLD = 4 << 20
MAX_DELETE = 65536
DEFAULT_MAX_RECORD = 1 << 30
_UNKNOWN_OUTCOME_CODES = frozenset({"timeout", "collection_unavailable"})
GREP_LINE_MAX = 2 << 20
GREP_SCOPES = frozenset({"payload", "headers", "all"})
GREP_EVENTS = frozenset({"hit", "unreadable", "progress", "end"})


def _media(resp: Any) -> str:
    return (resp.headers.get("Content-Type") or "").split(";")[0].strip().lower()


def _flag(q: List[Tuple[str, str]], name: str, value: bool) -> None:
    if value:
        q.append((name, "true"))


class Client:
    """Client for the Masstuffy HTTP API (docs/http-api.md). Thread-safe; see the README."""

    def __init__(self, base_url: str = "http://127.0.0.1:8470", *, timeout: float = 75.0, retries: int = 2,
                 backoff: float = 0.5, max_backoff: float = 30.0, pool_size: int = 4,
                 headers: Optional[Mapping[str, str]] = None, ssl_context: Optional[ssl.SSLContext] = None,
                 dict_cache_entries: int = 16, dict_cache_bytes: int = 128 << 20) -> None:
        self.timeout = timeout
        self._t = Transport(base_url, timeout=timeout, retries=retries, backoff=backoff, max_backoff=max_backoff,
                            pool_size=pool_size, headers=headers, ssl_context=ssl_context)
        self._status: Optional[ServerStatus] = None
        self._dict_cache: Optional[DictionaryCache] = None
        self._dict_args = (dict_cache_entries, dict_cache_bytes)
        self._lock = threading.Lock()

    def close(self) -> None:
        self._t.close()

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    # -- plumbing

    def _json(self, method: str, path: str, **kw: Any) -> Any:
        r = self._t.request(method, path, **kw)
        assert isinstance(r, Response)
        return r.json()

    # -- grep

    def grep(self, collections: Union[str, Sequence[str]],
             queries: Union[str, GrepQuery, Sequence[Union[str, GrepQuery]]], *, scope: str = "all",
             max_hits: Optional[int] = None, events: Sequence[str] = ("hit", "unreadable"), strict: bool = True,
             read_timeout: Optional[float] = 300.0) -> "GrepStream":
        """Search collections for queries (``POST /v1/grep``) and stream the matching records.

        ``collections`` is a slug or a sequence of slugs. ``queries`` is a string (a literal), a ``GrepQuery`` or a
        sequence of those; they are OR'ed and each hit lists the indices of the queries that matched. ``scope`` is
        ``"payload"``, ``"headers"`` or ``"all"``. ``events`` selects what the iterator yields, from ``"hit"``,
        ``"unreadable"`` and ``"progress"`` (``"end"`` is accepted and ignored: ``.report`` is always set). The
        request always asks for progress lines, which keep the connection alive and feed ``GrepStream.progress``.
        ``read_timeout`` is the socket timeout between two lines (None: the client's default).

        Returns a ``GrepStream`` (an iterator and a context manager; use ``with``). ValueError for a bad ``scope``
        or ``events`` value, before any request. With ``strict`` (default) the stream raises ``GrepIncompleteError``
        after its last item when a collection failed; truncation (``max_hits``) is never an error.
        """
        if scope not in GREP_SCOPES:
            raise ValueError("scope must be 'payload', 'headers' or 'all'")
        evs = frozenset((events,) if isinstance(events, str) else events)
        bad = evs - GREP_EVENTS
        if bad:
            raise ValueError("events must be among 'hit', 'unreadable', 'progress', 'end', not %r" % sorted(bad))
        cols = [collections] if isinstance(collections, str) else list(collections)
        if isinstance(queries, (str, GrepQuery)):
            queries = [queries]
        qs = []
        for q in queries:
            if isinstance(q, str):
                q = GrepQuery(q)
            elif not isinstance(q, GrepQuery):
                raise TypeError("a query must be a str or a GrepQuery, not %s" % type(q).__name__)
            qs.append(q.to_json())
        body: Dict[str, Any] = {"collections": cols, "queries": qs, "scope": scope,
                                "events": sorted((evs - {"end"}) | {"progress"})}
        if max_hits is not None:
            body["max_hits"] = max_hits
        resp = self._t.request("POST", "/v1/grep", json_body=body, stream=True, idempotent=True,
                               timeout=read_timeout)
        assert isinstance(resp, StreamingResponse)
        try:
            ctype = _media(resp)
            if ctype != "application/x-ndjson":
                raise ProtocolError("expected an NDJSON response, got Content-Type %r" % ctype)
        except BaseException:
            resp.close()
            raise
        return GrepStream(resp, evs, strict)

    # -- health / status

    def healthz(self) -> bool:
        """True when the server is healthy, False when it is shutting down."""
        try:
            self._json("GET", "/healthz")
            return True
        except UnavailableError as e:
            if e.code == "shutting_down":
                return False
            raise

    def status(self, *, refresh: bool = False) -> ServerStatus:
        """Server status (cached after the first call; ``refresh=True`` re-fetches)."""
        st = self._status
        if st is None or refresh:
            st = ServerStatus.from_json(self._json("GET", "/v1/status"))
            self._status = st
        return st

    def _max_record_bytes(self) -> int:
        try:
            return self.status().max_record_bytes
        except MasstuffyError:
            return DEFAULT_MAX_RECORD

    # -- collections

    def collections(self) -> List[CollectionInfo]:
        obj = self._json("GET", "/v1/collections")
        items = obj.get("collections") if isinstance(obj, dict) else None
        if not isinstance(items, list):
            raise ProtocolError("missing 'collections'")
        return [CollectionInfo.from_json(i) for i in items]

    def create_collection(self, slug: str, codec: Optional[Codec] = None) -> "Collection":
        if not slug:
            raise ValueError("slug must not be empty")
        body: Dict[str, Any] = {"slug": slug}
        if codec is not None:
            body["codec"] = codec.to_json()
        info = CollectionInfo.from_json(self._json("POST", "/v1/collections", json_body=body, ok=(201,)))
        return Collection(self, info.slug, info)

    def collection(self, slug: str) -> "Collection":
        """A handle on a collection (no request)."""
        if not slug:
            raise ValueError("slug must not be empty")
        return Collection(self, slug)

    def get_collection(self, slug: str) -> CollectionInfo:
        return CollectionInfo.from_json(self._json("GET", "/v1/collections/" + seg(_slug(slug))))

    def rename_collection(self, slug: str, new_slug: str) -> "Collection":
        info = CollectionInfo.from_json(
            self._json("PATCH", "/v1/collections/" + seg(_slug(slug)), json_body={"slug": new_slug}))
        return Collection(self, info.slug, info)

    def delete_collection(self, slug: str) -> DeletedCollection:
        return DeletedCollection.from_json(self._json("DELETE", "/v1/collections/" + seg(_slug(slug))))

    # -- global lookups

    def lookup_by_id(self, record_id: str, *, collection: Optional[str] = None,
                     sync: bool = False) -> Optional[Locator]:
        """Locator of the latest copy of a record, or None. Eventually consistent unless ``sync=True``."""
        q = self._id_query(record_id, collection, sync, "locator")
        return self._lookup_locator("/v1/lookup/by-id", q)

    def get_by_id(self, record_id: str, *, collection: Optional[str] = None, sync: bool = False) -> Optional[Record]:
        q = self._id_query(record_id, collection, sync, "record")
        return self._lookup_record("/v1/lookup/by-id", q)

    @staticmethod
    def _id_query(record_id: str, collection: Optional[str], sync: bool, fetch: str) -> List[Tuple[str, str]]:
        q = [("id", record_id)]
        if collection is not None:
            q.append(("collection", collection))
        q.append(("fetch", fetch))
        _flag(q, "sync", sync)
        return q

    def _lookup_locator(self, path: str, q: List[Tuple[str, str]]) -> Optional[Locator]:
        try:
            return Locator.from_json(self._json("GET", path, query=q))
        except NotFoundError as e:
            if e.code == "record_not_found":
                return None
            raise

    def _lookup_record(self, path: str, q: List[Tuple[str, str]]) -> Optional[Record]:
        try:
            resp = self._t.request("GET", path, query=q, stream=True)
        except NotFoundError as e:
            if e.code == "record_not_found":
                return None
            raise
        assert isinstance(resp, StreamingResponse)
        if _media(resp) != "application/warc":
            resp.close()
            raise ProtocolError("expected application/warc")
        return Record.from_stream(resp, locator=resp.headers.get("Masstuffy-Locator"), closer=resp.close)

    # -- index / jobs

    def sync_index(self) -> None:
        self._t.request("POST", "/v1/index/sync", idempotent=True, ok=(204,))

    def rebuild_index(self) -> "Job":
        return Job.from_json(self, self._json("POST", "/v1/index/rebuild", ok=(202,)))

    def jobs(self) -> List["Job"]:
        obj = self._json("GET", "/v1/jobs")
        items = obj.get("jobs") if isinstance(obj, dict) else None
        if not isinstance(items, list):
            raise ProtocolError("missing 'jobs'")
        return [Job.from_json(self, j) for j in items]

    def job(self, job_id: str) -> "Job":
        return Job.from_json(self, self._json("GET", "/v1/jobs/" + seg(job_id)))

    # -- dictionaries

    def dictionaries(self) -> List[DictionaryInfo]:
        obj = self._json("GET", "/v1/dictionaries")
        items = obj.get("dictionaries") if isinstance(obj, dict) else None
        if not isinstance(items, list):
            raise ProtocolError("missing 'dictionaries'")
        return [DictionaryInfo.from_json(i) for i in items]

    def dictionary(self, dict_id: int) -> bytes:
        """The raw ``.zstdict`` bytes (bypasses the cache). ``dict_id`` must be >= 1."""
        if isinstance(dict_id, bool) or not isinstance(dict_id, int) or dict_id < 1:
            raise ValueError("dict_id must be >= 1")
        r = self._t.request("GET", "/v1/dictionaries/%d" % dict_id)
        assert isinstance(r, Response)
        return r.data

    @property
    def dict_cache(self) -> DictionaryCache:
        with self._lock:
            if self._dict_cache is None:
                self._dict_cache = DictionaryCache(self.dictionary, max_entries=self._dict_args[0],
                                                   max_bytes=self._dict_args[1])
            return self._dict_cache


def _slug(slug: str) -> str:
    if not slug:
        raise ValueError("slug must not be empty")
    return slug


class _ImportFailed(Exception):
    """Internal: an import failed after ``committed`` records were committed (``uncertain``: maybe more)."""

    def __init__(self, cause: BaseException, committed: int, uncertain: bool, rep: Optional[ImportReport]) -> None:
        super().__init__(str(cause))
        self.cause = cause
        self.committed = committed
        self.uncertain = uncertain
        self.rep = rep


class _State:
    """Mutable counters shared between a body generator and its caller."""

    def __init__(self) -> None:
        self.count = 0
        self.size = 0
        self.pending: Optional[RawRecord] = None


class _Adapter:
    """A record the import path can stream: head, size, iter_raw()."""

    def __init__(self, head: bytes, size: int, chunks: Callable[[], Iterator[bytes]]) -> None:
        self.head = head
        self.size = size
        self.iter_raw = chunks


class _HashingWriter:
    def __init__(self, out: BinaryIO) -> None:
        self._out = out
        self._h = _xxhash().xxh3_64()
        self.count = 0

    def write(self, b: bytes) -> int:
        self._h.update(b)
        self.count += len(b)
        self._out.write(b)
        return len(b)

    def flush(self) -> None:
        pass

    def intdigest(self) -> int:
        return self._h.intdigest()


class _CountingOut:
    def __init__(self, out: BinaryIO) -> None:
        self._out = out
        self.count = 0

    def write(self, b: bytes) -> int:
        self._out.write(b)
        self.count += len(b)
        return len(b)

    def flush(self) -> None:
        self._out.flush()


class Collection:
    """A handle on one collection. Not thread-safe (use one handle per thread, or share the Client)."""

    def __init__(self, client: Client, slug: str, info: Optional[CollectionInfo] = None) -> None:
        self.client = client
        self.slug = slug
        self._info = info
        self._comp: Optional[Tuple[Tuple[Codec, Optional[int]], RecordCompressor]] = None

    @property
    def _base(self) -> str:
        return "/v1/collections/" + seg(self.slug)

    def _json(self, method: str, suffix: str, **kw: Any) -> Any:
        return self.client._json(method, self._base + suffix, **kw)

    # -- grep

    def grep(self, queries: Union[str, GrepQuery, Sequence[Union[str, GrepQuery]]], **kw: Any) -> "GrepStream":
        """``Client.grep`` on this collection only (same keyword arguments)."""
        return self.client.grep([self.slug], queries, **kw)

    # -- info / admin

    def info(self, *, refresh: bool = False) -> CollectionInfo:
        """Collection info (cached; ``refresh=True`` re-fetches, with stats)."""
        if self._info is None or refresh:
            self._info = self.client.get_collection(self.slug)
        return self._info

    def rename(self, new_slug: str) -> "Collection":
        return self.client.rename_collection(self.slug, new_slug)

    def delete_collection(self) -> DeletedCollection:
        return self.client.delete_collection(self.slug)

    def rebuild(self, codec: Codec, *, recompress: bool = False, on_unreadable: str = "fail",
                workers: Optional[int] = None, max_inflight_bytes: Optional[int] = None) -> "Job":
        body: Dict[str, Any] = {"codec": codec.to_json(), "recompress": recompress, "on_unreadable": on_unreadable}
        if workers is not None:
            body["workers"] = workers
        if max_inflight_bytes is not None:
            body["max_inflight_bytes"] = max_inflight_bytes
        return Job.from_json(self.client, self._json("POST", "/rebuild", json_body=body, ok=(202,)))

    def train(self, *, max_samples: Optional[int] = None, max_sample_bytes: Optional[int] = None,
              max_total_bytes: Optional[int] = None, max_dict_size: Optional[int] = None) -> "Job":
        body = {k: v for k, v in (("max_samples", max_samples), ("max_sample_bytes", max_sample_bytes),
                                  ("max_total_bytes", max_total_bytes), ("max_dict_size", max_dict_size))
                if v is not None}
        return Job.from_json(self.client, self._json("POST", "/train", json_body=body, ok=(202,)))

    # -- writes: server-side compression

    def put(self, records: Iterable[Union[RecordSpec, bytes, bytearray, memoryview]]) -> List[Locator]:
        """Store records in one request (one durable commit). Returns one locator per record."""
        items = list(records)
        if not items:
            raise ValueError("nothing to put")
        total, gen, replayable, _ = _put_body(items)
        resp = self.client._t.request("POST", self._base + "/records", body=gen if replayable else gen(),
                                      content_length=total, content_type="application/warc", ok=(201,))
        assert isinstance(resp, Response)
        locs = self._locators(resp)
        if all(isinstance(i, RecordSpec) for i in items) and len(locs) != len(items):
            raise ProtocolError("server returned %d locators for %d records" % (len(locs), len(items)))
        return locs

    @staticmethod
    def _locators(resp: Response) -> List[Locator]:
        obj = resp.json()
        recs = obj.get("records") if isinstance(obj, dict) else None
        if not isinstance(recs, list):
            raise ProtocolError("missing 'records'")
        return [Locator.from_json(r) for r in recs]

    def put_raw(self, data: Union[bytes, BinaryIO, Iterable[bytes]], *,
                content_length: Optional[int] = None) -> List[Locator]:
        """POST already serialized, uncompressed WARC data (bytes, binary file or iterable of bytes)."""
        kw: Dict[str, Any] = {}
        if isinstance(data, (bytes, bytearray, memoryview)):
            body: Any = data
        elif hasattr(data, "read"):
            fp = data
            if content_length is None:
                try:
                    pos = fp.tell()
                    content_length = fp.seek(0, os.SEEK_END) - pos
                    fp.seek(pos)
                except (AttributeError, OSError, ValueError):
                    content_length = None
            body = iter(lambda: fp.read(CHUNK), b"")
            kw["content_length"] = content_length
        else:
            body = data
            kw["content_length"] = content_length
        resp = self.client._t.request("POST", self._base + "/records", body=body,
                                      content_type="application/warc", ok=(201,), **kw)
        assert isinstance(resp, Response)
        return self._locators(resp)

    def put_warc_file(self, source: Any, *, compressed: Optional[bool] = None, mode: str = "put",
                      batch_bytes: Optional[int] = None, batch_records: Optional[int] = None,
                      start_record: int = 0,
                      on_batch: Optional[Callable[[List[Locator]], None]] = None) -> PutFileReport:
        """Upload every record of a WARC file (plain or gzip), forwarding the exact source bytes.

        ``mode="put"`` sends independent batches (one commit each, server-side compression);
        ``mode="import"`` streams the whole file as one client-compressed import. On failure raises
        PartialPutError whose report says where to resume (``start_record=report.next_record``).
        """
        if mode not in ("put", "import"):
            raise ValueError("mode must be 'put' or 'import'")
        if start_record < 0:
            raise ValueError("start_record must be >= 0")
        if not _framing._HAS_RAW_HEAD:
            raise MissingDependencyError("pywarc >= 0.4.1 is required (WarcBlock.raw_head)")
        if mode == "import":
            if on_batch is not None:
                raise ValueError("on_batch is not supported in import mode")
            return self._put_file_import(source, compressed, batch_bytes, batch_records, start_record)
        return self._put_file_put(source, compressed, batch_bytes, batch_records, start_record, on_batch)

    def _put_file_put(self, source: Any, compressed: Optional[bool], batch_bytes: Optional[int],
                      batch_records: Optional[int], start_record: int,
                      on_batch: Optional[Callable[[List[Locator]], None]]) -> PutFileReport:
        st = self.client.status()
        cap = st.limits.max_put_body_bytes
        bb = min(batch_bytes if batch_bytes is not None else 16 << 20, cap)
        br = min(batch_records if batch_records is not None else 1000, st.limits.max_put_records)
        if bb < 1 or br < 1:
            raise ValueError("batch_bytes and batch_records must be >= 1")
        max_rec = st.max_record_bytes

        def check(rec: RawRecord) -> None:
            if rec.size > cap:
                raise RecordTooLargeError(rec.ordinal, rec.size, cap)
            if rec.block.content_length > max_rec:
                raise RecordTooLargeError(rec.ordinal, rec.block.content_length, max_rec)

        records = batches = nbytes = 0
        next_record = start_record
        first = start_record
        state = _State()
        uncertain: Optional[Tuple[int, int]] = None

        def report() -> PutFileReport:
            return PutFileReport(records, batches, nbytes, next_record, uncertain)

        try:
            with open_warc_source(source, compressed) as (stream, comp):
                it = iter_raw_records(stream, compressed=comp, start=start_record)
                state.pending = next(it, None)
                while state.pending is not None:
                    first = state.pending.ordinal
                    state.count = state.size = 0
                    check(state.pending)

                    def gen() -> Iterator[bytes]:
                        rec = state.pending
                        while True:
                            assert rec is not None
                            state.count += 1
                            state.size += rec.size
                            yield from rec.iter_raw()
                            nxt = next(it, None)
                            if nxt is None:
                                state.pending = None
                                return
                            check(nxt)
                            if state.count >= br or state.size + nxt.size > bb:
                                state.pending = nxt
                                return
                            rec = nxt

                    resp = self.client._t.request("POST", self._base + "/records", body=gen(),
                                                  content_type="application/warc", ok=(201,))
                    assert isinstance(resp, Response)
                    # the 201 means the batch is durable: count it before parsing or calling back
                    records += state.count
                    batches += 1
                    nbytes += state.size
                    next_record = first + state.count
                    locs = self._locators(resp)
                    if len(locs) != state.count:
                        raise ProtocolError("server returned %d locators for %d records"
                                            % (len(locs), state.count))
                    if on_batch is not None:
                        on_batch(locs)
        except Exception as e:
            unknown = (isinstance(e, TransportError) and e.outcome_unknown) or \
                (isinstance(e, ServerError)) or (isinstance(e, UnavailableError) and e.code in _UNKNOWN_OUTCOME_CODES)
            if unknown and state.count:
                uncertain = (first, state.count)
            raise PartialPutError(report(), e) from e
        return report()

    # -- writes: client-side compression (raw import)

    def compressor(self, *, refresh: bool = False) -> RecordCompressor:
        """A RecordCompressor for the collection's current codec and raw_rules."""
        info = self.info(refresh=refresh)
        key = (info.codec, info.raw_rules.dict_id)
        if self._comp is None or self._comp[0] != key:
            self._comp = (key, RecordCompressor(self.client.dict_cache, info.codec, info.raw_rules))
        return self._comp[1]

    def import_records(self, records: Iterable[Union[RecordSpec, bytes]], *,
                       client_compress: bool = True) -> ImportReport:
        """Compress records on the client and send them as one raw import (returns a report, not locators:
        find them with ``lookup_by_id(..., sync=True)`` or ``scan``).

        ``bytes`` items must each hold exactly one WARC record. On a failure caused by a codec change, the
        collection info is refreshed and the remaining records are retried once.
        """
        items = list(records)
        if not items:
            raise ValueError("nothing to import")
        if not client_compress and self.info().codec.algo != "none":
            raise ValueError("server-side compression: use put()")
        prepared: List[Any] = []
        replayable = True
        for it in items:
            if isinstance(it, RecordSpec):
                p = _prepare_spec(it)
                replayable = replayable and p.replayable
                prepared.append(_Adapter(p.head, p.total, p.chunks))
            elif isinstance(it, (bytes, bytearray, memoryview)):
                prepared.append(_bytes_adapter(bytes(it)))
            else:
                raise TypeError("import_records items must be RecordSpec or bytes")

        class _Ctx:
            def __init__(self, recs: List[Any]) -> None:
                self.recs = recs

            def __enter__(self) -> Iterator[Any]:
                return iter(self.recs)

            def __exit__(self, *a: Any) -> None:
                return None

        try:
            report, _, _ = self._import_with_retry(lambda skip: _Ctx(prepared[skip:]), [("on_invalid", "abort")],
                                                   replayable, client_compress)
        except _ImportFailed as f:
            own = f.rep.records if f.rep is not None else 0
            if f.committed == own:
                raise f.cause from None  # nothing beyond what the error itself reports
            # records committed by an earlier attempt: the error alone would under-report them
            raise PartialPutError(ImportReport(None if f.rep is None else f.rep.format, f.committed,
                                               0 if f.rep is None else f.rep.batches, 0, [], -1), f.cause) from f.cause
        return report

    def _put_file_import(self, source: Any, compressed: Optional[bool], batch_bytes: Optional[int],
                         batch_records: Optional[int], start_record: int) -> PutFileReport:
        q: List[Tuple[str, str]] = [("on_invalid", "abort")]
        if batch_records is not None:
            q.append(("batch_records", str(batch_records)))
        if batch_bytes is not None:
            q.append(("batch_bytes", str(batch_bytes)))
        reopenable = isinstance(source, (str, os.PathLike))

        class _Ctx:
            def __init__(self, skip: int) -> None:
                self.skip = skip
                self._cm: Any = None

            def __enter__(self) -> Iterator[Any]:
                self._cm = open_warc_source(source, compressed)
                stream, comp = self._cm.__enter__()
                return iter_raw_records(stream, compressed=comp, start=start_record + self.skip)

            def __exit__(self, *a: Any) -> None:
                self._cm.__exit__(*a)

        try:
            report, _, sent = self._import_with_retry(_Ctx, q, reopenable, True)
        except _ImportFailed as f:
            nxt = start_record + f.committed
            rep = PutFileReport(f.committed, f.rep.batches if f.rep else 0, -1, nxt,
                                (nxt, -1) if f.uncertain else None)  # -1: records from nxt on may be committed
            raise PartialPutError(rep, f.cause) from f.cause
        return PutFileReport(report.records, report.batches, sent, start_record + report.records)

    def _import_with_retry(self, make_ctx: Callable[[int], Any], query: List[Tuple[str, str]], can_retry: bool,
                           client_compress: bool) -> Tuple[ImportReport, int, int]:
        """Run the import, retrying once after a codec change. Raises _ImportFailed carrying the number of
        records known committed (including the prefix of an earlier attempt)."""
        _xxhash()
        comp = self.compressor()
        if comp.codec.algo == "zstd":
            _zstd()
        skip = 0
        for attempt in (0, 1):
            started = [False]
            try:
                with make_ctx(skip) as recs:
                    started[0] = True
                    report, sent = self._send_import(recs, comp, query)
                if skip:
                    report = dataclasses.replace(report, records=report.records + skip)
                return report, skip, sent
            except Exception as e:
                rep = e.import_report if isinstance(e, ApiError) else None
                committed = skip + (rep.records if rep is not None else 0)
                # A non-API failure (transport, client side) after the request started: batches may have been
                # committed by the server without us knowing.
                uncertain = not isinstance(e, ApiError) and started[0]
                if attempt == 1 or not can_retry or not isinstance(e, ApiError):
                    raise _ImportFailed(e, committed, uncertain, rep) from e
                old = (comp.codec, comp.rules.dict_id)
                try:
                    new = self.info(refresh=True)
                    changed = (new.codec, new.raw_rules.dict_id) != old
                    if changed:
                        comp = self.compressor()
                        if comp.codec.algo == "zstd":
                            _zstd()
                except Exception as e2:
                    raise _ImportFailed(e, committed, not isinstance(e, ApiError), rep) from e2
                if not changed:
                    raise _ImportFailed(e, committed, False, rep) from e
                skip = committed
        raise AssertionError("unreachable")  # pragma: no cover

    def _send_import(self, recs: Iterator[Any], comp: RecordCompressor,
                     query: List[Tuple[str, str]]) -> Tuple[ImportReport, int]:
        state = _State()
        max_rec = self.client._max_record_bytes()
        resp = self.client._t.request("POST", self._base + "/import", query=query,
                                      body=_envelope_stream(recs, comp, state, max_rec),
                                      content_type="application/x-masstuffy-envelope")
        assert isinstance(resp, Response)
        return ImportReport.from_json(resp.json()), state.size

    def import_envelope(self, source: Union[bytes, str, "os.PathLike[str]", BinaryIO, Iterable[bytes]], *,
                        on_invalid: str = "abort", batch_records: Optional[int] = None,
                        batch_bytes: Optional[int] = None, resume_offset: int = 0) -> ImportReport:
        """POST a ready-made envelope (any format the server accepts), streamed."""
        if on_invalid not in ("abort", "skip"):
            raise ValueError("on_invalid must be 'abort' or 'skip'")
        if resume_offset < 0:
            raise ValueError("resume_offset must be >= 0")
        q: List[Tuple[str, str]] = [("on_invalid", on_invalid)]
        if batch_records is not None:
            q.append(("batch_records", str(batch_records)))
        if batch_bytes is not None:
            q.append(("batch_bytes", str(batch_bytes)))
        opened = None
        kw: Dict[str, Any] = {}
        try:
            if isinstance(source, (bytes, bytearray, memoryview)):
                data = bytes(source)
                if resume_offset:
                    nl = data.find(b"\n", 0, MAX_LINE)
                    first = data[:nl + 1] if nl >= 0 and data.startswith(b"#masstuffy-cdx/") else b""
                    data = first + data[resume_offset:]
                body: Any = data
            elif isinstance(source, (str, os.PathLike)) or hasattr(source, "read"):
                if isinstance(source, (str, os.PathLike)):
                    opened = open(source, "rb")
                    fp: Any = opened
                else:
                    fp = source
                try:
                    seekable = bool(fp.seekable())
                except AttributeError:
                    seekable = False
                if resume_offset and not seekable:
                    raise ValueError("resume_offset needs a seekable source")
                if seekable:
                    base = fp.tell()
                    end = fp.seek(0, os.SEEK_END)
                    first = b""
                    if resume_offset:
                        fp.seek(base)
                        line = fp.readline(MAX_LINE)
                        if line.startswith(b"#masstuffy-cdx/") and line.endswith(b"\n"):
                            first = line
                    start = base + resume_offset
                    fp.seek(start)
                    kw["content_length"] = len(first) + max(0, end - start)

                    def gen() -> Iterator[bytes]:
                        if first:
                            yield first
                        yield from iter(lambda: fp.read(CHUNK), b"")

                    body = gen()
                else:
                    body = iter(lambda: fp.read(CHUNK), b"")
            else:
                if resume_offset:
                    raise ValueError("resume_offset needs a seekable source")
                body = source
            resp = self.client._t.request("POST", self._base + "/import", query=q, body=body,
                                          content_type="application/x-masstuffy-envelope", **kw)
        finally:
            if opened is not None:
                opened.close()
        assert isinstance(resp, Response)
        return ImportReport.from_json(resp.json())

    # -- reads

    def get(self, locator: LocatorLike, *, decode: str = "server") -> Record:
        """Fetch one record. ``decode="client"`` fetches the stored bytes and decodes them locally."""
        tok = locator_token(locator)
        if decode == "client":
            raw = self.get_raw(tok, decompress=True)
            return Record.from_stream(raw, locator=tok, closer=raw.close)
        if decode != "server":
            raise ValueError("decode must be 'server' or 'client'")
        resp = self.client._t.request("GET", self._base + "/records/" + seg(tok), stream=True)
        assert isinstance(resp, StreamingResponse)
        if _media(resp) != "application/warc":
            resp.close()
            raise ProtocolError("expected application/warc")
        return Record.from_stream(resp, locator=tok, closer=resp.close)

    def get_raw(self, locator: LocatorLike, *, decompress: bool = False,
                verify_digest: Optional[bool] = None) -> RawBody:
        """The stored bytes of a record (``/raw``), optionally decompressed locally.

        The digest and size are verified at EOF when xxhash is installed (``verify_digest=True`` requires it).
        """
        tok = locator_token(locator)
        verify = have_xxhash() if verify_digest is None else verify_digest
        if verify:
            _xxhash()
        resp = self.client._t.request("GET", self._base + "/records/" + seg(tok) + "/raw", stream=True)
        assert isinstance(resp, StreamingResponse)
        try:
            h = resp.headers
            enc = h.get("Masstuffy-Encoding")
            if enc not in ("warc", "zstd"):
                raise ProtocolError("bad or missing Masstuffy-Encoding")
            raw_size = _int_header(h, "Masstuffy-Raw-Size", True)
            digest = h.get("Masstuffy-Digest")
            if digest is None or len(digest) != 16:
                raise ProtocolError("bad or missing Masstuffy-Digest")
            did: Optional[int] = None
            content_size = _int_header(h, "Masstuffy-Content-Size", False)
            if enc == "zstd":
                did = _int_header(h, "Masstuffy-Dict-Id", True)
            stream: Any = _Verifier(resp, digest, raw_size, verify)
            out_enc = enc
            if decompress and enc == "zstd":
                _zstd()
                assert did is not None
                dctx = self.client.dict_cache.decompressor(did)
                stream = open_decompressed(stream, dctx, content_size=content_size,
                                           max_output=self.client._max_record_bytes())
                out_enc = "warc"
        except BaseException:
            resp.close()
            raise
        return RawBody(stream, encoding=out_enc, dict_id=did, raw_size=raw_size, digest=digest,
                       content_size=content_size, closer=resp.close)

    def lookup_by_url(self, url: str, date: Union[None, str, datetime] = None, *,
                      sync: bool = False) -> Optional[Locator]:
        return self.client._lookup_locator(self._base + "/lookup/by-url", self._url_query(url, date, sync, "locator"))

    def get_by_url(self, url: str, date: Union[None, str, datetime] = None, *,
                   sync: bool = False) -> Optional[Record]:
        return self.client._lookup_record(self._base + "/lookup/by-url", self._url_query(url, date, sync, "record"))

    @staticmethod
    def _url_query(url: str, date: Union[None, str, datetime], sync: bool, fetch: str) -> List[Tuple[str, str]]:
        q = [("url", url)]
        if date is not None:
            q.append(("date", pywarc.format_warc_date(date) if isinstance(date, datetime) else date))
        q.append(("fetch", fetch))
        _flag(q, "sync", sync)
        return q

    def lookup_by_id(self, record_id: str, *, sync: bool = False) -> Optional[Locator]:
        return self.client.lookup_by_id(record_id, collection=self.slug, sync=sync)

    def get_by_id(self, record_id: str, *, sync: bool = False) -> Optional[Record]:
        return self.client.get_by_id(record_id, collection=self.slug, sync=sync)

    def search_pages(self, host: str, *, subdomains: bool = False, port: Union[None, str, int] = None,
                     path: Optional[str] = None, path_match: Optional[str] = None, limit: Optional[int] = None,
                     cursor: Optional[str] = None, sync: bool = False) -> Iterator[Page]:
        q: List[Tuple[str, str]] = [("host", host)]
        _flag(q, "subdomains", subdomains)
        if port is not None:
            if port in ("any", "default") or (isinstance(port, int) and not isinstance(port, bool)
                                              and 1 <= port <= 65535):
                q.append(("port", str(port)))
            else:
                raise ValueError("port must be 'any', 'default' or 1..65535")
        if path_match is not None:
            if path is None:
                raise ValueError("path_match needs path")
            if path_match not in ("prefix", "exact"):
                raise ValueError("path_match must be 'prefix' or 'exact'")
        if path is not None:
            q.append(("path", path))
        if path_match is not None:
            q.append(("path_match", path_match))
        if limit is not None:
            if not 1 <= limit <= 10000:
                raise ValueError("limit must be in 1..10000")
            q.append(("limit", str(limit)))
        _flag(q, "sync", sync)
        return self._pages("/search", q, "hits", cursor, False)

    def search(self, host: str, **kw: Any) -> Iterator[Locator]:
        for page in self.search_pages(host, **kw):
            yield from page.items

    def scan_pages(self, *, limit: Optional[int] = None, cursor: Optional[str] = None) -> Iterator[Page]:
        q: List[Tuple[str, str]] = []
        if limit is not None:
            if not 1 <= limit <= 10000:
                raise ValueError("limit must be in 1..10000")
            q.append(("limit", str(limit)))
        return self._pages("/scan", q, "items", cursor, True)

    def scan(self, *, limit: Optional[int] = None, cursor: Optional[str] = None) -> Iterator[Locator]:
        for page in self.scan_pages(limit=limit, cursor=cursor):
            yield from page.items

    def _pages(self, suffix: str, q: List[Tuple[str, str]], key: str, cursor: Optional[str],
               stop_on_empty: bool) -> Iterator[Page]:
        while True:
            qq = list(q)
            if cursor is not None:
                qq.append(("cursor", cursor))
            obj = self._json("GET", suffix, query=qq)
            items = obj.get(key) if isinstance(obj, dict) else None
            nxt = obj.get("next") if isinstance(obj, dict) else None
            if not isinstance(items, list) or not (nxt is None or isinstance(nxt, str)):
                raise ProtocolError("malformed page")
            page = Page([Locator.from_json(i) for i in items], nxt)
            if stop_on_empty:
                if not items:
                    return
                if nxt is None:
                    raise ProtocolError("non-empty scan page without a next cursor")
            yield page
            if nxt is None:
                return
            cursor = nxt

    # -- export

    def _export_pages(self, cursor: Optional[str], max_records: Optional[int], max_bytes: Optional[int],
                      on_unreadable: str) -> Iterator[Tuple[StreamingResponse, Optional[str], int, Optional[int]]]:
        """Yield (response, next_cursor, unreadable, record_count) for each non-empty page; the caller reads
        the body, and the response is closed when the generator advances."""
        if on_unreadable not in ("fail", "skip"):
            raise ValueError("on_unreadable must be 'fail' or 'skip'")
        while True:
            q: List[Tuple[str, str]] = []
            if cursor is not None:
                q.append(("cursor", cursor))
            if max_records is not None:
                q.append(("max_records", str(max_records)))
            if max_bytes is not None:
                q.append(("max_bytes", str(max_bytes)))
            if on_unreadable != "fail":
                q.append(("on_unreadable", on_unreadable))
            resp = self.client._t.request("GET", self._base + "/export", query=q, stream=True)
            assert isinstance(resp, StreamingResponse)
            try:
                nxt = resp.headers.get("Masstuffy-Next-Cursor")
                if nxt is None:
                    if resp.read(1) != b"":
                        raise ProtocolError("export page without a cursor must have an empty body")
                    return
                unreadable = _int_header(resp.headers, "Masstuffy-Unreadable-Count", False) or 0
                count = _int_header(resp.headers, "Masstuffy-Record-Count", False)
                yield resp, nxt, unreadable, count
            finally:
                resp.close()
            cursor = nxt

    def iter_export(self, *, cursor: Optional[str] = None, max_records: Optional[int] = None,
                    max_bytes: Optional[int] = None, on_unreadable: str = "fail") -> Iterator[Record]:
        """Yield the collection's records (decoded). A yielded Record is invalid once the iterator advances."""
        for resp, _, _, count in self._export_pages(cursor, max_records, max_bytes, on_unreadable):
            n = 0
            try:
                for rr in iter_raw_records(resp):
                    n += 1
                    yield Record(rr.block)
            except pywarc.InvalidWarcError as e:
                raise ProtocolError(f"malformed export page: {e}") from e
            if count is not None and n != count:
                raise ProtocolError("export page record count mismatch")

    def export(self, dest: Union[str, "os.PathLike[str]", BinaryIO], *, cursor: Optional[str] = None,
               max_records: Optional[int] = None, max_bytes: Optional[int] = None,
               on_unreadable: str = "fail", compress: Optional[bool] = None) -> ExportReport:
        """Write every record to ``dest`` (path: atomic via ``.part``; file object: written, not closed).

        ``compress`` (default: True iff ``dest`` is a path ending in ``.gz``) writes one gzip member per record.
        On failure raises ExportInterruptedError; its report covers complete pages only (resume with
        ``cursor=report.cursor``; with a file object, truncate it to ``report.bytes`` first). ``report.bytes`` is the absolute
        position in ``dest`` after the last complete page when ``dest`` is seekable (so appending to a non-empty
        file is safe), otherwise the number of bytes this call wrote.
        """
        if on_unreadable not in ("fail", "skip"):
            raise ValueError("on_unreadable must be 'fail' or 'skip'")
        is_path = isinstance(dest, (str, os.PathLike))
        if compress is None:
            compress = is_path and os.fspath(dest).endswith(".gz")  # type: ignore[arg-type]
        records = unreadable = pages = 0
        nbytes = 0
        last_cursor = cursor
        part = None
        fp: Any
        if is_path:
            path = os.fspath(dest)  # type: ignore[arg-type]
            part = path + ".part"
            fp = open(part, "wb")
        else:
            fp = dest
        out = _CountingOut(fp)
        base = 0   # report.bytes is an absolute position in ``dest`` when it is seekable
        if not is_path:
            try:
                if fp.seekable():
                    base = fp.tell()
            except (AttributeError, OSError, ValueError):
                base = 0
        nbytes = base
        ok = False
        try:
            try:
                for resp, nxt, unread, count in self._export_pages(cursor, max_records, max_bytes, on_unreadable):
                    n = 0
                    for rr in iter_raw_records(resp):
                        n += 1
                        if compress:
                            with gzip.GzipFile(fileobj=out, mode="wb", mtime=0) as gz:  # type: ignore[call-overload]
                                for c in rr.iter_raw():
                                    gz.write(c)
                        else:
                            for c in rr.iter_raw():
                                out.write(c)
                    if count is not None and n != count:
                        raise ProtocolError("export page record count mismatch")
                    records += n
                    unreadable += unread
                    pages += 1
                    nbytes = base + out.count
                    last_cursor = nxt
            except Exception as e:
                if isinstance(e, pywarc.InvalidWarcError):
                    e = _chain(ProtocolError(f"malformed export page: {e}"), e)
                raise ExportInterruptedError(ExportReport(records, unreadable, pages, nbytes, last_cursor), e) from e
            if is_path:
                fp.flush()
                os.fsync(fp.fileno())
                fp.close()
                os.replace(part, path)  # type: ignore[arg-type]
                part = None
            ok = True
        finally:
            if is_path:
                if not fp.closed:
                    fp.close()
                if part is not None and not ok:
                    try:
                        os.remove(part)
                    except OSError:
                        pass
        return ExportReport(records, unreadable, pages, nbytes, last_cursor)

    # -- deletes

    def delete(self, locators: Iterable[LocatorLike]) -> int:
        """Delete records (logically: tombstones). Returns the number deleted."""
        tokens = [locator_token(x) for x in locators]
        if len(tokens) > MAX_DELETE:
            raise ValueError("at most %d locators per call" % MAX_DELETE)
        return _deleted_count(self._json("POST", "/records/delete", json_body={"locators": tokens}))

    def delete_one(self, locator: LocatorLike) -> int:
        return _deleted_count(self._json("DELETE", "/records/" + seg(locator_token(locator))))

    def delete_by_id(self, record_id: str) -> Optional[Locator]:
        """Delete the latest copy of a record by id (index-backed; ``client.sync_index()`` first). None if unknown."""
        try:
            obj = self._json("POST", "/records/delete-by-id", json_body={"record_id": record_id})
        except NotFoundError as e:
            if e.code == "record_not_found":
                return None
            raise
        d = obj.get("deleted") if isinstance(obj, dict) else None
        if not isinstance(d, dict):
            raise ProtocolError("missing 'deleted'")
        return Locator.from_json(d)


def _deleted_count(obj: Any) -> int:
    n = obj.get("deleted") if isinstance(obj, dict) else None
    if isinstance(n, bool) or not isinstance(n, int):
        raise ProtocolError("missing or invalid 'deleted'")
    return n


def _chain(new: BaseException, cause: BaseException) -> BaseException:
    new.__cause__ = cause
    return new


def _int_header(h: Any, name: str, required: bool) -> Optional[int]:
    v = h.get(name)
    if v is None:
        if required:
            raise ProtocolError(f"missing header {name}")
        return None
    if not (v.isascii() and v.isdigit()):
        raise ProtocolError(f"bad header {name}: {v!r}")
    return int(v)


class _Verifier:
    """Reads a StreamingResponse, checking the size and xxh3 digest of the stored bytes at EOF."""

    def __init__(self, resp: StreamingResponse, digest: str, size: int, verify: bool) -> None:
        self._r = resp
        self._digest = digest
        self._size = size
        self._h = _xxhash().xxh3_64() if verify else None
        self._count = 0
        self._done = False

    def read(self, n: int = -1) -> bytes:
        data = self._r.read(n)
        self._count += len(data)
        if self._h is not None:
            self._h.update(data)
        if (n is None or n < 0 or (not data and n > 0)) and not self._done:
            self._done = True
            if self._count != self._size:
                raise ProtocolError("stored body size differs from Masstuffy-Raw-Size")
            if self._h is not None and self._h.hexdigest() != self._digest.lower():
                raise ProtocolError("stored body digest differs from Masstuffy-Digest")
        return data


def _bytes_adapter(b: bytes) -> _Adapter:
    try:
        recs = list(_one_record(b))
    except pywarc.InvalidWarcError as e:
        raise ValueError(f"item is not a valid WARC record: {e}") from e
    if len(recs) != 1:
        raise ValueError("each bytes item must hold exactly one WARC record")
    head, size = recs[0]
    return _Adapter(head, size, lambda: iter([b]))


def _one_record(b: bytes) -> Iterator[Tuple[bytes, int]]:
    for r in iter_raw_records(io.BytesIO(b)):
        yield r.head, r.size


def _envelope_stream(recs: Iterable[Any], comp: RecordCompressor, state: _State, max_record: int) -> Iterator[bytes]:
    """Generate an import envelope: client-side compression, small records in memory, large ones spooled."""
    yield ENVELOPE_HEADER
    xx = _xxhash()
    ordinal = 0
    for rec in recs:
        meta = meta_from_head(rec.head)
        content = rec.size
        if content > max_record:
            raise RecordTooLargeError(ordinal, content, max_record)
        if content <= SPOOL_THRESHOLD:
            raw = b"".join(rec.iter_raw())
            payload = comp.compress(raw)
            line = v1_line(len(payload), xx.xxh3_64_intdigest(payload), meta.date, meta.warc_type,
                           meta.record_id, meta.url)
            yield line
            yield payload
            state.size += len(raw)
        else:
            with tempfile.SpooledTemporaryFile(max_size=SPOOL_THRESHOLD) as spool:
                w = _HashingWriter(spool)
                comp.compress_stream(rec.iter_raw(), content, w)  # type: ignore[arg-type]
                yield v1_line(w.count, w.intdigest(), meta.date, meta.warc_type, meta.record_id, meta.url)
                spool.seek(0)
                while True:
                    c = spool.read(CHUNK)
                    if not c:
                        break
                    yield c
            state.size += content
        state.count += 1
        ordinal += 1


class GrepStream:
    """The result of a grep: an iterator of ``GrepHit`` / ``GrepUnreadable`` (and ``GrepProgress`` when asked),
    and a context manager that owns the connection.

    ``.report`` is the ``GrepReport`` once the ``end`` line arrived (None before). ``.progress`` maps a slug to its
    latest ``GrepProgress`` seen so far (tracked even when progress is not yielded).

    Judge completeness from ``report.collections[i].status`` only; ``GrepProgress.fraction`` is a progress
    indicator, never a completeness test. A response that ends without an ``end`` line raises ``ProtocolError``.
    One thread per stream. ``close()`` drops the connection, which detaches the grep on the server.
    """

    def __init__(self, resp: StreamingResponse, events: Any, strict: bool) -> None:
        self._resp: Optional[StreamingResponse] = resp
        self._events = frozenset(events)
        self._strict = strict
        self._done = False
        self.report: Optional[GrepReport] = None
        self.progress: Dict[str, GrepProgress] = {}

    def __iter__(self) -> "GrepStream":
        return self

    def __enter__(self) -> "GrepStream":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def close(self) -> None:
        """Close the connection (it is not reused). Safe to call more than once."""
        resp, self._resp = self._resp, None
        if resp is not None:
            resp.close()

    def _line(self) -> Dict[str, Any]:
        resp = self._resp
        if resp is None:
            raise StopIteration
        raw = resp.readline(GREP_LINE_MAX)
        if not raw:
            raise ProtocolError("grep stream ended without an end line")
        if not raw.endswith(b"\n"):
            raise ProtocolError("grep stream line too long or cut (no newline)")
        try:
            obj = json.loads(raw)
        except (ValueError, UnicodeDecodeError) as e:
            raise ProtocolError("invalid JSON line in grep stream: %s" % e) from e
        if not isinstance(obj, dict) or not isinstance(obj.get("type"), str):
            raise ProtocolError("grep stream line without a type")
        return obj

    def __next__(self) -> Union[GrepHit, GrepUnreadable, GrepProgress]:
        if self._done:
            raise StopIteration
        try:
            while True:
                try:
                    obj = self._line()
                except StopIteration:
                    self._done = True
                    raise
                kind = obj["type"]
                if kind == "hit":
                    hit = GrepHit.from_json(obj)
                    if "hit" in self._events:
                        return hit
                elif kind == "unreadable":
                    unreadable = GrepUnreadable.from_json(obj)
                    if "unreadable" in self._events:
                        return unreadable
                elif kind == "progress":
                    prog = GrepProgress.from_json(obj)
                    self.progress[prog.slug] = prog
                    if "progress" in self._events:
                        return prog
                elif kind == "end":
                    self.report = GrepReport.from_json(obj)
                    self._done = True
                    self.close()
                    if self._strict and any(c.status == "failed" for c in self.report.collections):
                        raise GrepIncompleteError(self.report)
                    raise StopIteration
                else:
                    raise ProtocolError("unknown grep line type %r" % kind)
        except StopIteration:
            raise
        except BaseException:
            self._done = True
            self.close()
            raise


class Job:
    """A background job (rebuild, train, index rebuild). Not thread-safe."""

    def __init__(self, client: Client, obj: Dict[str, Any]) -> None:
        self.client = client
        self._update(obj)

    @classmethod
    def from_json(cls, client: Client, obj: Dict[str, Any]) -> "Job":
        return cls(client, obj)

    def _update(self, obj: Dict[str, Any]) -> None:
        if not isinstance(obj, dict) or not isinstance(obj.get("id"), (str, int)) or \
                not isinstance(obj.get("state"), str) or not isinstance(obj.get("kind"), str):
            raise ProtocolError("malformed job")
        self.id = str(obj["id"])
        self.kind: str = obj["kind"]
        self.collection = obj.get("collection")
        self.collection_id = obj.get("collection_id")
        self.state: str = obj["state"]
        self.started_at = obj.get("started_at")
        self.finished_at = obj.get("finished_at")
        self.result = obj.get("result")
        err = obj.get("error")
        self.error: Optional[ApiError] = error_from_object(err) if isinstance(err, dict) else None

    @property
    def done(self) -> bool:
        return self.state != "running"

    def refresh(self, wait: float = 0) -> "Job":
        """Re-fetch the job; with ``wait`` (0..60 s) the server long-polls while it is running."""
        q: List[Tuple[str, str]] = []
        w = 0
        if wait > 0:
            w = min(60, max(1, int(math.ceil(wait))))
            q.append(("wait", str(w)))
        self._update(self.client._json("GET", "/v1/jobs/" + seg(self.id), query=q,
                                       timeout=self.client.timeout + w))
        return self

    def wait(self, timeout: Optional[float] = None, *, raise_on_failure: bool = True) -> "Job":
        """Block until the job finishes. JobTimeoutError at the deadline; JobFailedError if it failed."""
        deadline = None if timeout is None else time.monotonic() + timeout
        while not self.done:
            if deadline is None:
                w = 60.0
            else:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise JobTimeoutError(self)
                w = min(60.0, remaining)
            self.refresh(w)
        if self.state == "failed" and raise_on_failure:
            raise JobFailedError(self, self.error)
        return self
