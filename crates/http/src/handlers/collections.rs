//! Health, status and collection routes.

use std::sync::Arc;

use axum::extract::RawQuery;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use masstuffy::Error;
use serde_json::json;

use super::{collection, json_response, path_err};
use crate::body::collect_limited;
use crate::error::ApiError;
use crate::json::{
    CreateCollectionJson, DeletedCollectionJson, RenameJson, collection_json, parse_json,
    raw_check_str,
};
use crate::params::percent_encode;
use crate::router::AppState;

type St = State<Arc<AppState>>;
type Slug = Result<Path<String>, PathRejection>;

/// `GET /healthz`.
pub(crate) async fn healthz(State(st): St, RawQuery(q): RawQuery) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    // `""` is never a valid slug and `collection()` checks the closed flag first, so this is a
    // side-effect-free liveness probe through the public API.
    match st.repo.collection("") {
        Err(Error::ShuttingDown) => Err(Error::ShuttingDown.into()),
        _ => json_response(StatusCode::OK, &json!({"status": "ok"})),
    }
}

/// `GET /v1/status`.
pub(crate) async fn status(State(st): St, RawQuery(q): RawQuery) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    let o = &st.opts;
    let dictionaries = st.repo.dictionaries().map_or(0, |d| d.len());
    json_response(
        StatusCode::OK,
        &json!({
            "version": env!("CARGO_PKG_VERSION"),
            "api": 1,
            "repository": st.repo.path().display().to_string(),
            "raw_check": raw_check_str(st.repo.raw_check()),
            "max_record_bytes": st.repo.config().max_record_bytes,
            "collections": st.repo.collections().len(),
            "dictionaries": dictionaries,
            "jobs_running": st.jobs.running(),
            "limits": {
                "max_put_body_bytes": o.max_put_body_bytes,
                "max_put_records": o.max_put_records,
                "max_import_body_bytes": o.max_import_body_bytes,
                "max_json_body_bytes": o.max_json_body_bytes,
                "max_export_page_bytes": o.max_export_page_bytes,
                "request_timeout_ms": u64::try_from(o.request_timeout.as_millis()).unwrap_or(u64::MAX),
                "body_idle_timeout_ms": u64::try_from(o.body_idle_timeout.as_millis()).unwrap_or(u64::MAX),
                "max_concurrent_requests": o.max_concurrent_requests,
            },
            "open_report": st.open_report,
        }),
    )
}

/// `GET /v1/collections`.
pub(crate) async fn list(State(st): St, RawQuery(q): RawQuery) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    let cols: Vec<_> = st
        .repo
        .collections()
        .iter()
        .map(|i| collection_json(i, None))
        .collect();
    json_response(StatusCode::OK, &json!({ "collections": cols }))
}

/// `POST /v1/collections`.
pub(crate) async fn create(State(st): St, req: Request) -> Result<Response, ApiError> {
    super::no_params(req.uri().query())?;
    let body = collect_limited(
        req.into_body(),
        st.opts.max_json_body_bytes,
        st.opts.body_idle_timeout,
    )
    .await?;
    let r: CreateCollectionJson = parse_json(&body)?;
    let codec = match r.codec {
        Some(c) => c.to_codec()?,
        None => masstuffy::Codec::None,
    };
    let h = st.repo.create_collection(&r.slug, codec).await?;
    let mut resp = json_response(
        StatusCode::CREATED,
        &collection_json(&h.info(), Some(h.stats())),
    )?;
    if let Ok(v) = HeaderValue::from_str(&format!("/v1/collections/{}", percent_encode(&h.slug())))
    {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    Ok(resp)
}

/// `GET /v1/collections/{slug}`.
pub(crate) async fn get_one(
    State(st): St,
    slug: Slug,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    let Path(slug) = slug.map_err(path_err)?;
    let h = collection(&st.repo, &slug)?;
    json_response(StatusCode::OK, &collection_json(&h.info(), Some(h.stats())))
}

/// `PATCH /v1/collections/{slug}`.
pub(crate) async fn rename(State(st): St, slug: Slug, req: Request) -> Result<Response, ApiError> {
    super::no_params(req.uri().query())?;
    let Path(slug) = slug.map_err(path_err)?;
    collection(&st.repo, &slug)?;
    let body = collect_limited(
        req.into_body(),
        st.opts.max_json_body_bytes,
        st.opts.body_idle_timeout,
    )
    .await?;
    let r: RenameJson = parse_json(&body)?;
    let h = st.repo.rename_collection(slug.as_str(), &r.slug).await?;
    json_response(StatusCode::OK, &collection_json(&h.info(), Some(h.stats())))
}

/// `DELETE /v1/collections/{slug}`.
pub(crate) async fn delete(
    State(st): St,
    slug: Slug,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    let Path(slug) = slug.map_err(path_err)?;
    let d = st.repo.delete_collection(slug.as_str()).await?;
    json_response(StatusCode::OK, &DeletedCollectionJson::from(&d))
}
