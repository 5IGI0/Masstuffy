"""Exception hierarchy of the Masstuffy client and the mapping from server error bodies."""

from __future__ import annotations

import json
from typing import Any, Dict, Mapping, Optional

__all__ = [
    "MasstuffyError", "TransportError", "ProtocolError", "MissingDependencyError", "ApiError",
    "InvalidRequestError", "PayloadTooLargeError", "NotFoundError", "ConflictError",
    "StaleLocatorError", "NotDecodableError", "UnavailableError", "ServerError",
    "JobFailedError", "JobTimeoutError", "PartialPutError", "ExportInterruptedError",
    "RecordTooLargeError", "GrepIncompleteError", "error_from_response", "error_from_object",
]


class MasstuffyError(Exception):
    """Base class of every error raised by this package (except plain ValueError/TypeError
    for bad arguments)."""


class TransportError(MasstuffyError):
    """Connection refused/reset, socket timeout, truncated response, TLS error.

    ``outcome_unknown`` is True when the request body was fully sent (or there was none) before the
    failure and the method is not idempotent: the mutation may have happened.
    """

    def __init__(self, message: str, *, outcome_unknown: bool = False) -> None:
        super().__init__(message)
        self.outcome_unknown = outcome_unknown


class ProtocolError(MasstuffyError):
    """The server answered something the contract forbids (bad JSON, missing header, malformed WARC...)."""


class MissingDependencyError(MasstuffyError, ImportError):
    """An optional dependency is not installed (the message names the extra)."""


class ApiError(MasstuffyError):
    """An error reported by the server (``{"error": {kind, code, message, details}}``)."""

    def __init__(self, status: Optional[int], kind: str, code: str, message: str,
                 details: Optional[Dict[str, Any]] = None, retry_after: Optional[float] = None,
                 method: Optional[str] = None, path: Optional[str] = None) -> None:
        super().__init__(message)
        self.status = status
        self.kind = kind
        self.code = code
        self.message = message
        self.details = details
        self.retry_after = retry_after
        self.method = method
        self.path = path

    @property
    def import_report(self):  # -> Optional[ImportReport]
        """The import report attached to the error (``details.report``), if any."""
        if not isinstance(self.details, dict):
            return None
        rep = self.details.get("report")
        if not isinstance(rep, dict):
            return None
        from .models import ImportReport  # local import: models imports errors

        try:
            return ImportReport.from_json(rep)
        except ProtocolError:
            return None

    def __str__(self) -> str:
        if self.status is None:
            return f"{self.code}: {self.message}"
        return f"{self.status} {self.code}: {self.message}"


class InvalidRequestError(ApiError):
    """kind ``invalid`` (400, 405, 408, 413, 415)."""


class PayloadTooLargeError(InvalidRequestError):
    """Status 413 (``payload_too_large``, ``too_many_records``)."""


class NotFoundError(ApiError):
    """kind ``not_found``."""


class ConflictError(ApiError):
    """kind ``conflict``."""


class StaleLocatorError(ConflictError):
    """code ``stale_locator``: the collection was rebuilt; resolve the record again."""


class NotDecodableError(ApiError):
    """code ``not_decodable`` (422): the stored payload disagrees with its metadata; read it with get_raw."""


class UnavailableError(ApiError):
    """kind ``unavailable`` (503)."""


class ServerError(ApiError):
    """kind corrupt / io / internal, or any other 5xx."""


class JobFailedError(MasstuffyError):
    """``Job.wait(raise_on_failure=True)`` on a failed job. ``.job``, ``.error`` (ApiError)."""

    def __init__(self, job: Any, error: Optional[ApiError]) -> None:
        super().__init__(f"job {getattr(job, 'id', '?')} failed: {error}")
        self.job = job
        self.error = error


class JobTimeoutError(MasstuffyError):
    """``Job.wait`` deadline reached; ``.job`` is still running."""

    def __init__(self, job: Any) -> None:
        super().__init__(f"job {getattr(job, 'id', '?')} still running at the deadline")
        self.job = job


class PartialPutError(MasstuffyError):
    """put_warc_file / import failure after some records were acknowledged. ``.report``, ``.cause``."""

    def __init__(self, report: Any, cause: BaseException) -> None:
        super().__init__(f"partial put: {cause}")
        self.report = report
        self.cause = cause


class ExportInterruptedError(MasstuffyError):
    """An export failed. ``.report`` describes only the complete pages; ``.cause`` is the error."""

    def __init__(self, report: Any, cause: BaseException) -> None:
        super().__init__(f"export interrupted: {cause}")
        self.report = report
        self.cause = cause


class RecordTooLargeError(MasstuffyError, ValueError):
    """Client-side pre-check: a record exceeds a server limit. ``.ordinal``, ``.size``, ``.limit``."""

    def __init__(self, ordinal: int, size: int, limit: int) -> None:
        super().__init__(f"record {ordinal} is {size} bytes, over the limit of {limit}")
        self.ordinal = ordinal
        self.size = size
        self.limit = limit


class GrepIncompleteError(MasstuffyError):
    """A grep finished (its ``end`` line arrived) but at least one collection has status ``failed``.

    ``.report`` is the full ``GrepReport``; hits already yielded are valid but the result is partial.
    """

    def __init__(self, report: Any) -> None:
        failed = [c for c in getattr(report, "collections", []) if getattr(c, "status", "") == "failed"]
        detail = "; ".join(f"{c.slug}: {c.error}" for c in failed)
        super().__init__(f"grep incomplete ({len(failed)} collection(s) failed): {detail}")
        self.report = report


def _parse_retry_after(headers: Optional[Mapping[str, str]]) -> Optional[float]:
    if headers is None:
        return None
    v = headers.get("Retry-After")
    if v is None:
        return None
    v = v.strip()
    if v.isascii() and v.isdigit():
        return float(int(v))
    return None


def _pick_class(status: Optional[int], kind: str, code: str) -> type:
    if code == "stale_locator":
        return StaleLocatorError
    if code == "not_decodable":
        return NotDecodableError
    if status == 413:
        return PayloadTooLargeError
    by_kind = {
        "invalid": InvalidRequestError,
        "not_found": NotFoundError,
        "conflict": ConflictError,
        "unavailable": UnavailableError,
    }
    if kind in by_kind:
        return by_kind[kind]
    if status is None or status >= 500:
        return ServerError
    if status == 404:
        return NotFoundError
    if status == 409:
        return ConflictError
    if status == 503:
        return UnavailableError
    return InvalidRequestError


def error_from_response(status: int, body: bytes, headers: Optional[Mapping[str, str]],
                        method: Optional[str], path: Optional[str]) -> ApiError:
    """Build the typed ApiError for an HTTP error response."""
    retry_after = _parse_retry_after(headers)
    err = None
    try:
        obj = json.loads(body)
        err = obj.get("error") if isinstance(obj, dict) else None
    except (ValueError, UnicodeDecodeError):
        err = None
    if not (isinstance(err, dict) and isinstance(err.get("kind"), str) and isinstance(err.get("code"), str)):
        cls = ServerError if status >= 500 else InvalidRequestError
        msg = bytes(body[:200]).decode("utf-8", "replace")
        return cls(status, "http", f"http_{status}", msg, None, retry_after, method, path)
    details = err.get("details")
    if not isinstance(details, dict):
        details = None
    message = err.get("message")
    if not isinstance(message, str):
        message = ""
    cls = _pick_class(status, err["kind"], err["code"])
    return cls(status, err["kind"], err["code"], message, details, retry_after, method, path)


def error_from_object(obj: Dict[str, Any]) -> ApiError:
    """Build the typed ApiError for an error object that is not an HTTP response (failed job)."""
    kind = obj.get("kind") if isinstance(obj.get("kind"), str) else "internal"
    code = obj.get("code") if isinstance(obj.get("code"), str) else "internal"
    message = obj.get("message") if isinstance(obj.get("message"), str) else ""
    details = obj.get("details") if isinstance(obj.get("details"), dict) else None
    cls = _pick_class(None, kind, code)
    return cls(None, kind, code, message, details, None, None, None)
