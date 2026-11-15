//! Route handlers.

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use masstuffy::{CollectionHandle, CollectionId, Located, Repository};
use serde::Serialize;

use crate::error::ApiError;
use crate::locator::decode_locator;

pub(crate) mod admin;
pub(crate) mod collections;
pub(crate) mod dicts;
pub(crate) mod query;
pub(crate) mod records;

pub(crate) const H_RECORD_ID: HeaderName = HeaderName::from_static("masstuffy-record-id");
pub(crate) const H_LOCATOR: HeaderName = HeaderName::from_static("masstuffy-locator");
pub(crate) const H_LOCATOR_OMITTED: HeaderName =
    HeaderName::from_static("masstuffy-locator-omitted");
pub(crate) const H_ENCODING: HeaderName = HeaderName::from_static("masstuffy-encoding");
pub(crate) const H_DICT_ID: HeaderName = HeaderName::from_static("masstuffy-dict-id");
pub(crate) const H_RAW_SIZE: HeaderName = HeaderName::from_static("masstuffy-raw-size");
pub(crate) const H_DIGEST: HeaderName = HeaderName::from_static("masstuffy-digest");
pub(crate) const H_CONTENT_SIZE: HeaderName = HeaderName::from_static("masstuffy-content-size");
pub(crate) const H_RECORD_COUNT: HeaderName = HeaderName::from_static("masstuffy-record-count");
pub(crate) const H_UNREADABLE_COUNT: HeaderName =
    HeaderName::from_static("masstuffy-unreadable-count");
pub(crate) const H_NEXT_CURSOR: HeaderName = HeaderName::from_static("masstuffy-next-cursor");

/// Longest header value we emit for ids and tokens.
pub(crate) const MAX_HEADER_VALUE: usize = 8192;

pub(crate) const WARC: &str = "application/warc";

/// A path rejection as our error format.
pub(crate) fn path_err(e: PathRejection) -> ApiError {
    ApiError::invalid("invalid_param", format!("invalid path parameter: {e}"))
}

/// A JSON response with the given status.
pub(crate) fn json_response<T: Serialize>(status: StatusCode, v: &T) -> Result<Response, ApiError> {
    let bytes = serde_json::to_vec(v)
        .map_err(|e| ApiError::internal(format!("cannot encode response: {e}")))?;
    let mut r = Response::new(Body::from(bytes));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(r)
}

/// A response with `content_type` and the given bytes (moved, not copied).
pub(crate) fn bytes_response(
    status: StatusCode,
    content_type: &'static str,
    bytes: Vec<u8>,
) -> Response {
    let mut r = Response::new(Body::from(bytes));
    *r.status_mut() = status;
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    r
}

/// Inserts `name: value` if the value is a valid header value of reasonable size.
pub(crate) fn set_header(r: &mut Response, name: HeaderName, value: &str) -> bool {
    if value.len() > MAX_HEADER_VALUE {
        return false;
    }
    match HeaderValue::from_str(value) {
        Ok(v) => {
            r.headers_mut().insert(name, v);
            true
        }
        Err(_) => false,
    }
}

pub(crate) fn set_header_num(r: &mut Response, name: HeaderName, n: u64) {
    r.headers_mut().insert(name, HeaderValue::from(n));
}

/// Resolves a collection by slug (404 `unknown_collection`, 503 `shutting_down`).
pub(crate) fn collection(repo: &Repository, slug: &str) -> Result<CollectionHandle, ApiError> {
    repo.collection(slug).map_err(ApiError::from)
}

/// Decodes a token and checks that it names `handle`'s collection.
pub(crate) fn locator_for(handle: &CollectionHandle, token: &str) -> Result<Located, ApiError> {
    let at =
        decode_locator(token).map_err(|e| ApiError::invalid("invalid_locator", e.to_string()))?;
    check_collection(handle.id(), &at)?;
    Ok(at)
}

pub(crate) fn check_collection(id: CollectionId, at: &Located) -> Result<(), ApiError> {
    if at.collection == id {
        Ok(())
    } else {
        Err(ApiError::invalid(
            "locator_collection_mismatch",
            "locator belongs to another collection",
        ))
    }
}

/// Rejects any query parameter on a route that takes none.
pub(crate) fn no_params(query: Option<&str>) -> Result<(), ApiError> {
    crate::params::Params::parse(query, &[]).map(|_| ())
}
