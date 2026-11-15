"""Python client for the Masstuffy HTTP API. See the README."""

from ._version import __version__
from .client import Client, Collection, Job
from .compression import DictionaryCache, RecordCompressor
from .envelope import EnvelopeWriter
from .errors import (ApiError, ConflictError, ExportInterruptedError, InvalidRequestError, JobFailedError,
                     JobTimeoutError, MasstuffyError, MissingDependencyError, NotDecodableError, NotFoundError,
                     PartialPutError, PayloadTooLargeError, ProtocolError, RecordTooLargeError, ServerError,
                     StaleLocatorError, TransportError, UnavailableError)
from .models import (Codec, CollectionInfo, CollectionStats, DeletedCollection, DictionaryInfo, ExportReport,
                     ImportReport, Limits, Locator, LocatorLike, Page, PutFileReport, RawRules, Rejection,
                     ServerStatus)
from .records import RawBody, Record, RecordSpec

__all__ = [
    "__version__", "Client", "Collection", "Job", "Record", "RawBody", "RecordSpec", "Locator", "LocatorLike",
    "Codec", "RawRules", "CollectionInfo", "CollectionStats", "DeletedCollection", "ServerStatus", "Limits",
    "DictionaryInfo", "ImportReport", "Rejection", "Page", "PutFileReport", "ExportReport", "EnvelopeWriter",
    "DictionaryCache", "RecordCompressor", "MasstuffyError", "TransportError", "ProtocolError",
    "MissingDependencyError", "ApiError", "InvalidRequestError", "PayloadTooLargeError", "NotFoundError",
    "ConflictError", "StaleLocatorError", "NotDecodableError", "UnavailableError", "ServerError",
    "JobFailedError", "JobTimeoutError", "PartialPutError", "ExportInterruptedError", "RecordTooLargeError",
]
