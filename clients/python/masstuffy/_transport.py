"""HTTP transport: a small thread-safe connection pool over ``http.client``, retries and streaming responses.

All network I/O of the package goes through this module.
"""

from __future__ import annotations

import collections
import http.client
import json
import select
import socket
import ssl
import threading
import time
import urllib.parse
from typing import Any, Callable, Deque, Iterable, Iterator, Mapping, Optional, Sequence, Tuple, Union

from ._version import __version__
from .errors import ApiError, ProtocolError, TransportError, UnavailableError, error_from_response

__all__ = ["Transport", "Response", "StreamingResponse", "seg"]

IDLE_MAX_AGE = 15.0
RETRY_CODES = frozenset({"overloaded", "too_many_jobs", "index_lagging", "too_many_greps"})
ERROR_BODY_MAX = 1 << 20

Body = Union[None, bytes, bytearray, memoryview, Iterable[bytes], Callable[[], Iterable[bytes]]]


def seg(s: str) -> str:
    """Quote one dynamic path segment."""
    return urllib.parse.quote(s, safe="")


def _readable(sock: Optional[socket.socket]) -> bool:
    """True when an idle socket has pending data or EOF (so it must not be reused)."""
    if sock is None:
        return True
    try:
        if hasattr(select, "poll"):
            p = select.poll()
            p.register(sock.fileno(), select.POLLIN | select.POLLERR | select.POLLHUP)
            return bool(p.poll(0))
        return bool(select.select([sock], [], [], 0)[0])
    except (OSError, ValueError):
        return True


class Response:
    """A fully read response."""

    def __init__(self, status: int, headers: Any, data: bytes) -> None:
        self.status = status
        self.headers = headers
        self.data = data

    def json(self) -> Any:
        ctype = (self.headers.get("Content-Type") or "").split(";")[0].strip().lower()
        if ctype != "application/json":
            raise ProtocolError(f"expected a JSON response, got Content-Type {ctype!r}")
        try:
            return json.loads(self.data)
        except (ValueError, UnicodeDecodeError) as e:
            raise ProtocolError(f"invalid JSON in response: {e}") from e


class StreamingResponse:
    """A response body read incrementally. Owns one connection until closed or exhausted.

    Use as a context manager. Closing before the end closes the connection (the rest is never drained).
    Offers the file protocol pywarc needs (``read``, ``readline``, ``seekable``).
    """

    def __init__(self, resp: http.client.HTTPResponse, release: Callable[[bool], None]) -> None:
        self._resp = resp
        self._release = release
        self._closed = False
        self.status = resp.status
        self.headers = resp.headers

    def _wrap(self, fn: Callable[..., bytes], *args: Any) -> bytes:
        try:
            return fn(*args)
        except http.client.IncompleteRead as e:
            raise TransportError("response body truncated") from e
        except (OSError, http.client.HTTPException) as e:
            raise TransportError(f"error reading response: {e}") from e

    def read(self, n: Optional[int] = -1) -> bytes:
        if self._closed:
            return b""
        if n is None or n < 0:
            return self._wrap(self._resp.read)
        return self._wrap(self._resp.read, n)

    def readline(self, limit: Optional[int] = -1) -> bytes:
        if self._closed:
            return b""
        if limit is None or limit < 0:
            return self._wrap(self._resp.readline)
        return self._wrap(self._resp.readline, limit)

    def readable(self) -> bool:
        return True

    def seekable(self) -> bool:
        return False

    @property
    def at_eof(self) -> bool:
        return self._closed or self._resp.isclosed() or self._resp.length == 0

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        at_eof = self._resp.isclosed() or self._resp.length == 0
        if at_eof and not self._resp.isclosed():
            self._resp.close()  # fully read (e.g. empty body): lets http.client accept the next request
        self._release(at_eof)

    def __enter__(self) -> "StreamingResponse":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            pass


class _BodyBox:
    """Tracks what happened to an iterable request body."""

    def __init__(self) -> None:
        self.fully_sent = False
        self.exc: Optional[BaseException] = None

    def wrap(self, it: Iterable[bytes]) -> Iterator[bytes]:
        try:
            for chunk in it:
                if chunk:
                    yield chunk
        except BaseException as e:
            self.exc = e
            raise
        self.fully_sent = True


class Transport:
    def __init__(self, base_url: str, *, timeout: float = 75.0, retries: int = 2, backoff: float = 0.5,
                 max_backoff: float = 30.0, pool_size: int = 4, headers: Optional[Mapping[str, str]] = None,
                 ssl_context: Optional[ssl.SSLContext] = None) -> None:
        parts = urllib.parse.urlsplit(base_url)
        if parts.scheme not in ("http", "https"):
            raise ValueError("base_url must start with http:// or https://")
        if parts.query or parts.fragment or not parts.hostname:
            raise ValueError("base_url must be scheme://host[:port][/prefix] without query or fragment")
        self._scheme = parts.scheme
        self._host = parts.hostname
        self._port = parts.port or (443 if parts.scheme == "https" else 80)
        self._prefix = parts.path.rstrip("/")
        self.timeout = timeout
        self._retries = max(0, retries)
        self._backoff = backoff
        self._max_backoff = max_backoff
        self._pool_size = pool_size
        self._headers = dict(headers or {})
        self._ssl = ssl_context
        self._idle: Deque[Tuple[http.client.HTTPConnection, float]] = collections.deque()
        self._lock = threading.Lock()
        self._closed = False

    # -- pool

    def _new_conn(self, timeout: float) -> http.client.HTTPConnection:
        if self._scheme == "https":
            return http.client.HTTPSConnection(self._host, self._port, timeout=timeout, context=self._ssl,
                                               blocksize=65536)
        return http.client.HTTPConnection(self._host, self._port, timeout=timeout, blocksize=65536)

    def _acquire(self, timeout: float) -> http.client.HTTPConnection:
        while True:
            with self._lock:
                entry = self._idle.pop() if self._idle else None
            if entry is None:
                return self._new_conn(timeout)
            conn, last = entry
            if time.monotonic() - last > IDLE_MAX_AGE or conn.sock is None or _readable(conn.sock):
                conn.close()
                continue
            conn.timeout = timeout
            conn.sock.settimeout(timeout)
            return conn

    def _release(self, conn: http.client.HTTPConnection, reusable: bool) -> None:
        if reusable and not self._closed and conn.sock is not None:
            with self._lock:
                if len(self._idle) < self._pool_size:
                    self._idle.append((conn, time.monotonic()))
                    return
        conn.close()

    def idle_count(self) -> int:
        with self._lock:
            return len(self._idle)

    def close(self) -> None:
        self._closed = True
        with self._lock:
            idle, self._idle = list(self._idle), collections.deque()
        for conn, _ in idle:
            conn.close()

    # -- requests

    def request(self, method: str, path: str, *, query: Sequence[Tuple[str, str]] = (), json_body: Any = None,
                body: Body = None, content_length: Optional[int] = None, content_type: Optional[str] = None,
                headers: Optional[Mapping[str, str]] = None, stream: bool = False,
                idempotent: Optional[bool] = None, timeout: Optional[float] = None,
                ok: Tuple[int, ...] = (200,)) -> Union[Response, StreamingResponse]:
        """Send a request and return a Response (or a StreamingResponse with ``stream=True``).

        ``body`` may be bytes-like, an iterable of bytes (not replayable, never retried) or a callable that
        returns a fresh iterable (replayable). Raises ApiError for a status not in ``ok``, TransportError
        for network failures.
        """
        if json_body is not None:
            body = json.dumps(json_body, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
            content_type = "application/json"
        if idempotent is None:
            idempotent = method in ("GET", "HEAD")
        replayable = body is None or isinstance(body, (bytes, bytearray, memoryview)) or callable(body)
        url = self._prefix + path
        if query:
            url += "?" + urllib.parse.urlencode(list(query), quote_via=urllib.parse.quote)
        hdrs = {"User-Agent": "masstuffy-python/" + __version__, "Accept-Encoding": "identity"}
        hdrs.update(self._headers)
        hdrs.update(headers or {})
        if content_type:
            hdrs["Content-Type"] = content_type
        eff_timeout = self.timeout if timeout is None else timeout
        attempt = 0
        while True:
            try:
                return self._once(method, url, path, body, content_length, hdrs, stream, idempotent, eff_timeout,
                                  ok)
            except UnavailableError as e:
                if not replayable or attempt >= self._retries or e.code not in RETRY_CODES:
                    raise
                delay = e.retry_after if e.retry_after is not None else self._backoff * 2 ** attempt
            except TransportError:
                if not (idempotent and replayable) or attempt >= self._retries:
                    raise
                delay = self._backoff * 2 ** attempt
            time.sleep(min(self._max_backoff, delay))
            attempt += 1

    def _once(self, method, url, path, body, content_length, hdrs, stream, idempotent, timeout, ok):
        conn = self._acquire(timeout)
        box = _BodyBox()
        send_body: Any = body
        hdrs = dict(hdrs)
        if callable(send_body):
            send_body = send_body()
        if send_body is None:
            if method in ("POST", "PATCH", "PUT", "DELETE"):
                hdrs["Content-Length"] = "0"
        elif isinstance(send_body, (bytes, bytearray, memoryview)):
            pass
        else:
            send_body = box.wrap(send_body)
            if content_length is not None:
                hdrs["Content-Length"] = str(content_length)
        bytes_body = send_body is None or isinstance(send_body, (bytes, bytearray, memoryview))
        resp = None
        dirty = False   # the request was not fully sent: the connection must not be pooled
        try:
            try:
                conn.request(method, url, body=send_body, headers=hdrs)
                if bytes_body:
                    box.fully_sent = True
            except BaseException as e:
                if box.exc is not None:
                    conn.close()
                    raise box.exc
                if not isinstance(e, (OSError, http.client.HTTPException)):
                    conn.close()
                    raise
                # The server may have answered early (413/415/404...) and closed.
                dirty = True
                try:
                    resp = conn.getresponse()
                except (OSError, http.client.HTTPException):
                    resp = None
                if resp is None or resp.status < 400:
                    conn.close()
                    raise TransportError(f"{method} {path}: {e}",
                                         outcome_unknown=False) from e
            if resp is None:
                resp = conn.getresponse()
        except TransportError:
            raise
        except BaseException as e:
            if box.exc is not None and e is box.exc:
                raise  # error raised by the caller's own body iterator: surface it unchanged
            if not isinstance(e, (OSError, http.client.HTTPException)):
                conn.close()
                raise
            conn.close()
            if isinstance(e, socket.timeout):
                raise TransportError(f"{method} {path}: timed out",
                                     outcome_unknown=(not idempotent) and box.fully_sent) from e
            raise TransportError(f"{method} {path}: {e}",
                                 outcome_unknown=(not idempotent) and box.fully_sent) from e

        if resp.status not in ok:
            try:
                data = resp.read(ERROR_BODY_MAX)
            except (OSError, http.client.HTTPException) as e:
                conn.close()
                raise TransportError(f"{method} {path}: error reading error body: {e}") from e
            self._release(conn, resp.isclosed() and not resp.will_close and not dirty)
            raise error_from_response(resp.status, data, resp.headers, method, path)

        if stream:
            def release(at_eof: bool, conn=conn, resp=resp) -> None:
                self._release(conn, at_eof and not resp.will_close)

            return StreamingResponse(resp, release)
        try:
            data = resp.read()
        except http.client.IncompleteRead as e:
            conn.close()
            raise TransportError(f"{method} {path}: response truncated",
                                 outcome_unknown=not idempotent) from e
        except (OSError, http.client.HTTPException) as e:
            conn.close()
            raise TransportError(f"{method} {path}: {e}", outcome_unknown=not idempotent) from e
        self._release(conn, not resp.will_close)
        return Response(resp.status, resp.headers, data)
