//! Error responses: status, JSON body, and the mapping from engine errors.

use std::fmt;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use masstuffy::{Error, ErrorKind};
use serde_json::{Value, json};

use crate::json::import_report_json;

/// Marks a response built from an `ApiError`, so the request logger can report the code.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ErrorCode(pub &'static str);

/// An HTTP error response: a status and the JSON body
/// `{"error": {"kind", "code", "message", "details"?}}`.
///
/// It is a response value, not a library error type.
#[derive(Debug)]
pub struct ApiError {
    /// HTTP status.
    pub status: StatusCode,
    /// Coarse class (`not_found`, `conflict`, `invalid`, `corrupt`, `io`, `unavailable`, `internal`).
    pub kind: &'static str,
    /// Stable machine-readable code.
    pub code: &'static str,
    /// Human-readable text.
    pub message: String,
    /// Structured extra data, if any.
    pub details: Option<Value>,
    /// Seconds for the `Retry-After` header.
    pub retry_after: Option<u32>,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    fn new(
        status: StatusCode,
        kind: &'static str,
        code: &'static str,
        message: impl Into<String>,
    ) -> ApiError {
        ApiError {
            status,
            kind,
            code,
            message: message.into(),
            details: None,
            retry_after: None,
        }
    }

    pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid", code, message)
    }

    pub(crate) fn not_found(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", code, message)
    }

    pub(crate) fn method_not_allowed() -> ApiError {
        ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "invalid",
            "method_not_allowed",
            "method not allowed for this route",
        )
    }

    pub(crate) fn body_timeout() -> ApiError {
        ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "invalid",
            "body_timeout",
            "no request body data received within the idle timeout",
        )
    }

    pub(crate) fn body_read(message: impl Into<String>) -> ApiError {
        ApiError::invalid("body_read", message)
    }

    pub(crate) fn too_large(message: impl Into<String>) -> ApiError {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid",
            "payload_too_large",
            message,
        )
    }

    pub(crate) fn unsupported_media(message: impl Into<String>) -> ApiError {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid",
            "unsupported_media_type",
            message,
        )
    }

    pub(crate) fn unavailable(
        code: &'static str,
        message: impl Into<String>,
        retry_after: u32,
    ) -> ApiError {
        let mut e = ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            code,
            message,
        );
        e.retry_after = Some(retry_after);
        e
    }

    pub(crate) fn internal(message: impl Into<String>) -> ApiError {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal",
            message,
        )
    }

    pub(crate) fn with_details(mut self, details: Value) -> ApiError {
        self.details = Some(details);
        self
    }

    /// Adds `key: value` to the details object (creating it).
    pub(crate) fn with_detail(mut self, key: &str, value: Value) -> ApiError {
        match &mut self.details {
            Some(Value::Object(m)) => {
                m.insert(key.to_owned(), value);
            }
            _ => {
                let mut m = serde_json::Map::new();
                m.insert(key.to_owned(), value);
                self.details = Some(Value::Object(m));
            }
        }
        self
    }

    /// `{"kind", "code", "message", "details"?}`.
    pub(crate) fn object(&self) -> Value {
        let mut o = json!({
            "kind": self.kind,
            "code": self.code,
            "message": self.message,
        });
        if let (Some(d), Some(m)) = (&self.details, o.as_object_mut()) {
            m.insert("details".to_owned(), d.clone());
        }
        o
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&json!({ "error": self.object() }))
            .unwrap_or_else(|_| br#"{"error":{"kind":"internal","code":"internal"}}"#.to_vec());
        let mut resp = Response::new(axum::body::Body::from(body));
        *resp.status_mut() = self.status;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Some(s) = self.retry_after {
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(s));
        }
        resp.extensions_mut().insert(ErrorCode(self.code));
        resp
    }
}

fn kind_status(k: ErrorKind) -> (StatusCode, &'static str) {
    match k {
        ErrorKind::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        ErrorKind::Conflict => (StatusCode::CONFLICT, "conflict"),
        ErrorKind::Invalid => (StatusCode::BAD_REQUEST, "invalid"),
        ErrorKind::Corrupt => (StatusCode::INTERNAL_SERVER_ERROR, "corrupt"),
        ErrorKind::Io => (StatusCode::INTERNAL_SERVER_ERROR, "io"),
        ErrorKind::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    }
}

/// `(code, details)` of an engine error.
fn code_details(e: &Error) -> (&'static str, Option<Value>) {
    match e {
        Error::UnknownCollection(_) => ("unknown_collection", None),
        Error::NotARepository { .. } => ("not_a_repository", None),
        Error::DictNotFound(id) => ("dict_not_found", Some(json!({ "dict_id": id }))),
        Error::Locked { .. } => ("locked", None),
        Error::NotEmpty { .. } => ("not_empty", None),
        Error::AlreadyInitialized { .. } => ("already_initialized", None),
        Error::SlugTaken { .. } => ("slug_taken", None),
        Error::StaleLocator { expected, got } => (
            "stale_locator",
            Some(json!({ "expected": expected, "got": got })),
        ),
        Error::RebuildInProgress(_) => ("rebuild_in_progress", None),
        Error::RebuildAborted { .. } => ("rebuild_aborted", None),
        Error::StaleGeneration(_) => ("stale_generation", None),
        Error::Rebuild { committed, .. } => {
            ("rebuild_failed", Some(json!({ "committed": committed })))
        }
        Error::Layout { .. } => ("layout", None),
        Error::NotDecodable { .. } => ("not_decodable", None),
        Error::Reopen { .. } => ("reopen_failed", None),
        Error::Read { .. } => ("read_failed", None),
        Error::Collection { .. } => ("collection_error", None),
        Error::Dict(_) => ("dict_store", None),
        Error::IndexQuery(_) => ("index_query", None),
        Error::Envelope(_) => ("invalid_envelope", None),
        Error::Import { .. } => ("import_failed", None),
        Error::InvalidQuery(_) => ("invalid_query", None),
        Error::Config { .. } => ("config", None),
        Error::InvalidEnv { .. } => ("invalid_env", None),
        Error::InvalidSlug { .. } => ("invalid_slug", None),
        Error::Train(_) => ("train_failed", None),
        Error::InvalidOptions(_) => ("invalid_options", None),
        Error::Build { index, .. } | Error::Prepare { index, .. } => {
            ("invalid_record", Some(json!({ "index": index })))
        }
        Error::Tombstone { index, .. } => {
            ("invalid_delete_target", Some(json!({ "index": index })))
        }
        // `kind()` is `Invalid` only for a tombstone whose locator was never acknowledged.
        Error::Commit { .. } if e.kind() == ErrorKind::Invalid => ("locator_not_committed", None),
        Error::Commit { .. } => ("commit_failed", None),
        Error::Io { .. } | Error::Durable(_) => ("io", None),
        Error::IndexOpen { .. } => ("index_open", None),
        Error::ShuttingDown => ("shutting_down", None),
        Error::CollectionUnavailable(id) => (
            "collection_unavailable",
            Some(json!({ "collection": id.to_string(), "outcome": "unknown" })),
        ),
        Error::Join(_) | Error::Internal(_) => ("internal", None),
        Error::ShutdownFailed(_) => ("shutdown_failed", None),
        Error::IndexLagging { collections } => {
            let ids: Vec<String> = collections.iter().map(ToString::to_string).collect();
            ("index_lagging", Some(json!({ "collections": ids })))
        }
        Error::IndexUnavailable => ("index_unavailable", None),
        _ => ("error", None),
    }
}

impl From<&Error> for ApiError {
    fn from(e: &Error) -> ApiError {
        if let Error::Import { report, source } = e {
            let cause = ApiError::from(&**source);
            let mut out = ApiError::new(cause.status, cause.kind, "import_failed", e.to_string());
            out.retry_after = cause.retry_after;
            let mut c = cause.object();
            if let Some(m) = c.as_object_mut() {
                // The nested details are not repeated: the cause keeps kind, code and message.
                m.remove("details");
            }
            out.details = Some(json!({
                "report": import_report_json(report),
                "cause": c,
            }));
            return out;
        }
        let (status, kind) = match e {
            Error::NotDecodable { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "corrupt"),
            _ => kind_status(e.kind()),
        };
        let (code, details) = code_details(e);
        let mut out = ApiError::new(status, kind, code, e.to_string());
        out.details = details;
        if matches!(e, Error::IndexLagging { .. }) {
            out.retry_after = Some(1);
        }
        out
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> ApiError {
        ApiError::from(&e)
    }
}

#[cfg(test)]
mod tests {
    use masstuffy::{CollectionId, ImportReport};

    use super::*;

    fn map(e: Error) -> ApiError {
        ApiError::from(e)
    }

    #[test]
    fn error_kind_status_table() {
        let a = map(Error::UnknownCollection("x".into()));
        assert_eq!(
            (a.status.as_u16(), a.kind, a.code),
            (404, "not_found", "unknown_collection")
        );
        let a = map(Error::SlugTaken { slug: "x".into() });
        assert_eq!(
            (a.status.as_u16(), a.kind, a.code),
            (409, "conflict", "slug_taken")
        );
        let a = map(Error::Layout {
            missing: "/x".into(),
        });
        assert_eq!(
            (a.status.as_u16(), a.kind, a.code),
            (500, "corrupt", "layout")
        );
        let a = map(Error::InvalidQuery("limit"));
        assert_eq!(
            (a.status.as_u16(), a.kind, a.code),
            (400, "invalid", "invalid_query")
        );
        let a = map(Error::Io {
            op: "read",
            path: "/x".into(),
            source: std::io::Error::other("boom"),
        });
        assert_eq!((a.status.as_u16(), a.kind, a.code), (500, "io", "io"));
        let a = map(Error::ShuttingDown);
        assert_eq!(
            (a.status.as_u16(), a.kind, a.code),
            (503, "unavailable", "shutting_down")
        );
        let a = map(Error::StaleLocator {
            expected: 2,
            got: 1,
        });
        assert_eq!(a.status.as_u16(), 409);
        assert_eq!(a.details.unwrap()["expected"], 2);
        let a = map(Error::InvalidOptions("x"));
        assert_eq!((a.status.as_u16(), a.code), (400, "invalid_options"));
    }

    #[test]
    fn not_decodable_is_422() {
        let a = map(Error::NotDecodable {
            source: masstuffy_collection::ReadError::EntryMismatch("record_id"),
        });
        assert_eq!(
            (a.status.as_u16(), a.kind, a.code),
            (422, "corrupt", "not_decodable")
        );
    }

    #[test]
    fn index_lagging_has_retry_after() {
        let a = map(Error::IndexLagging {
            collections: vec![CollectionId::from_u128(1)],
        });
        assert_eq!((a.status.as_u16(), a.retry_after), (503, Some(1)));
        assert_eq!(a.code, "index_lagging");
        let resp = a.into_response();
        assert_eq!(resp.headers().get("retry-after").unwrap(), "1");
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn import_error_takes_source_status() {
        let report = ImportReport {
            records: 7,
            ..ImportReport::default()
        };
        let a = map(Error::Import {
            report: Box::new(report),
            source: Box::new(Error::ShuttingDown),
        });
        assert_eq!((a.status.as_u16(), a.code), (503, "import_failed"));
        let d = a.details.unwrap();
        assert_eq!(d["report"]["records"], 7);
        assert_eq!(d["cause"]["code"], "shutting_down");
    }

    #[test]
    fn error_body_shape() {
        let a = ApiError::invalid("invalid_param", "bad");
        assert_eq!(
            a.object(),
            json!({"kind": "invalid", "code": "invalid_param", "message": "bad"})
        );
        let a = a.with_details(json!({"index": 1}));
        let o = a.object();
        assert_eq!(o["details"]["index"], 1);
        assert_eq!(a.to_string(), "invalid_param: bad");
    }
}
