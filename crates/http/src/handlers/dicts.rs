//! Dictionary listing and download (clients compress and decompress with them).

use std::sync::Arc;

use axum::extract::RawQuery;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::json;

use super::{H_DICT_ID, bytes_response, json_response, path_err, set_header_num};
use crate::error::ApiError;
use crate::params::parse_canonical;
use crate::router::AppState;

type St = State<Arc<AppState>>;

/// `GET /v1/dictionaries`.
pub(crate) async fn list(State(st): St, RawQuery(q): RawQuery) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    let dicts: Vec<_> = st
        .repo
        .dictionaries()?
        .into_iter()
        .map(|d| json!({"dict_id": d.dict_id, "label": d.label, "size": d.size}))
        .collect();
    json_response(StatusCode::OK, &json!({ "dictionaries": dicts }))
}

/// True if `If-None-Match` names `etag` (comma-separated list compared per item, `*` matches).
fn matches_etag(req: &Request, etag: &str) -> bool {
    req.headers()
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|t| t == "*" || t == etag)
}

/// `GET /v1/dictionaries/{dict_id}`.
pub(crate) async fn download(
    State(st): St,
    id: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<Response, ApiError> {
    super::no_params(req.uri().query())?;
    let Path(id) = id.map_err(path_err)?;
    let id: u32 = parse_canonical(&id)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| ApiError::invalid("invalid_param", "dict_id: expected a decimal u32"))?;
    let bytes = st.repo.dictionary(id).await?;
    // Hashing up to `dict_max_file_size` bytes: off the runtime workers.
    let (bytes, digest) = tokio::task::spawn_blocking(move || {
        let d = masstuffy_cdx::digest(&bytes);
        (bytes, d)
    })
    .await
    .map_err(|e| ApiError::internal(format!("hash task failed: {e}")))?;
    let etag = format!("\"{digest:016x}\"");
    let mut r = if matches_etag(&req, &etag) {
        let mut r = Response::new(axum::body::Body::empty());
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        r
    } else {
        bytes_response(StatusCode::OK, "application/octet-stream", bytes)
    };
    set_header_num(&mut r, H_DICT_ID, u64::from(id));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    Ok(r)
}
